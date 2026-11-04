//! One `Transmit` (W5a, chunks 9a, 9b): the ledger first, D6 in the backend (D7), the registry's one frame per local
//! per connection (D5), the outcome recorded before the answer (D4), the error table (D6), the ack that does not
//! come in time (the connection's end, the late ack: D5). Over a [`FakeSession`] and a
//! real store; the ack is handed to the registry the way the event handler does. Synthetic ids only (the fictitious
//! 555-01xx range and made-up LIDs, each tail of 8 digits a substring of no other value); texts start with
//! "synthetic".

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::DbKey;
use kourier_model::{Body, ConvKey, ErrorKind, MsgTarget, ReplyTo, TargetAuthor, Text, UserId};

use super::{ACK_WAIT, Files, Gate, HOLD_RETRY, Parts, START_ON_PHONE, Written, transmit};
use crate::map::OwnIds;
use crate::send::acks::{Ack, Acks, Answer, Nack, RATE_LIMITED_FLOOR, TRANSIENT_FLOOR, Waited};
use crate::send::address::Namespace;
use crate::send::hold::Recheck;
use crate::send::ledger::{self, Entry, Kind, Outcome};
use crate::send::started::{self, How};
use crate::send::stop::{Registered, Stop};
use crate::send::tokens::TOKEN_RETRY;
use crate::session::fake::{Call, FakeSession};
use crate::session::{Devices, Groups, SendFailure, Session};
use crate::store::{KourierStore, StoreOwner};

const PEER_PN: &str = "15550162947308";
const STRANGER_PN: &str = "15550118375642";
const OWN_PN: &str = "15550173029584";
const OWN_LID: &str = "77700164820395";
const GROUP: &str = "120363000083920471";
const LOCAL: &str = "3EB0B7C61D2E48F95031";
const OTHER_LOCAL: &str = "3EB0A2F49C7D13E68524";
const ORIGINAL: &str = "3EB0D5A81C4F7E2B9063";
const AUTHOR_PN: &str = "15550195206837";
const AUTHOR_LID: &str = "77700151683092";
const NOW: i64 = 1_761_500_000_000;

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

fn req(conv: &ConvKey, local: &str, kind: OutgoingKind) -> OutgoingRequest {
    OutgoingRequest {
        conv: conv.clone(),
        local: local.into(),
        sent_at: NOW - 1_000,
        kind,
        only: None,
        is_recipient_update: false,
    }
}

fn text(conv: &ConvKey, local: &str) -> OutgoingRequest {
    req(
        conv,
        local,
        OutgoingKind::Message {
            body: Body::Text(Text::plain("synthetic hello")),
            reply_to: None,
            expiry: None,
        },
    )
}

struct Rig {
    _dir: tempfile::TempDir,
    owner: StoreOwner,
    session: Arc<FakeSession>,
    acks: Arc<Acks>,
}

impl Rig {
    /// A store whose chat with [`PEER_PN`] the phone started.
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("dir");
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &DbKey::from([11; 32]),
            "wa-attempt",
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
            acks: Arc::default(),
        }
    }

    fn store(&self) -> KourierStore {
        self.owner.store()
    }

    fn parts(&self) -> Parts {
        let session: Arc<dyn Session> = self.session.clone();
        Parts {
            store: self.store(),
            session: Some(session),
            acks: self.acks.clone(),
            own: own(),
            now_ms: NOW,
            hold: Recheck::fixed(Gate::Open),
            linked_ms: None,
            stop: Registered::alone(Stop::closed()),
            files: Files::none(),
        }
    }

    /// The attempt of `req`, running; the handle ends with its ANSWER (the attempt may go on after it).
    fn spawn(&self, req: OutgoingRequest) -> tokio::task::JoinHandle<TransmitOutcome> {
        spawn_with(self.parts(), req)
    }

    /// Waits until the session was asked `n` times to drop its connection.
    async fn drops(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.session.count(&Call::DropConnection) < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the connection is dropped");
    }

    /// Waits until the registry holds `n` waits of the live connection for `id`.
    async fn waits(&self, id: &str, n: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.acks.waiting(id) < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the waits are registered");
    }

    /// Waits until `n` frames were given to the session.
    async fn frames(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.session.sent().len() < n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the frames are written");
    }

    fn ack(&self, id: &str, answer: Answer) {
        self.acks.answer(Ack {
            id: id.into(),
            answer,
        });
    }

    /// The ledger's outcome of `local` (found by the local alone, fix F13; `_conv` names the chat the test is about).
    async fn looked(&self, _conv: &ConvKey, local: &str) -> Option<Outcome> {
        let local = local.to_owned();
        self.store()
            .read(move |conn| ledger::look(conn, &local))
            .await
            .expect("look")
    }
}

/// The attempt of `req` with `parts`, running; the handle ends with its answer.
fn spawn_with(parts: Parts, req: OutgoingRequest) -> tokio::task::JoinHandle<TransmitOutcome> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    tokio::spawn(transmit(parts, req, reply));
    tokio::spawn(async move { answer.await.expect("the attempt answers") })
}

fn failed_kind(outcome: &TransmitOutcome) -> ErrorKind {
    match outcome {
        TransmitOutcome::Failed(e) => e.kind.clone(),
        other => panic!("the send did not fail: {other:?}"),
    }
}

/// D2/D4: the frame goes out under the core's local, and the answer `Sent{server_at}` comes only after the ack is
/// in the ledger. Mutants: the ledger's commit skipped; the server time not carried.
#[tokio::test]
async fn a_send_is_written_under_its_local_and_answered_after_its_ack_is_recorded() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    let frame = &rig.session.sent()[0];
    assert_eq!(frame.id, LOCAL, "the stanza id is the core's local (D2)");
    assert_eq!(frame.devices, Devices::Cached);
    assert!(
        rig.looked(&conv, LOCAL).await.is_none(),
        "nothing is recorded before the ack"
    );
    rig.ack(
        LOCAL,
        Answer::Acked {
            server_t: Some(1_761_500_004),
        },
    );
    let outcome = attempt.await.expect("the attempt ends");
    assert_eq!(
        outcome,
        TransmitOutcome::Sent {
            server_at: Some(1_761_500_004_000),
            per_recipient: Vec::new(),
        }
    );
    assert_eq!(
        rig.looked(&conv, LOCAL).await,
        Some(Outcome::Acked {
            server_t: Some(1_761_500_004)
        }),
        "the ack must be in the ledger when the answer leaves"
    );
}

/// D4: an outcome the ledger has is the answer, with NO frame: acked -> `Sent` with its time, refused ->
/// `ReachOutRefused`. Mutant: the ledger look skipped.
#[tokio::test]
async fn a_recorded_outcome_is_the_answer_and_no_frame_is_written() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    for (local, outcome) in [
        (
            LOCAL,
            Outcome::Acked {
                server_t: Some(1_761_500_009),
            },
        ),
        (OTHER_LOCAL, Outcome::Refused),
    ] {
        ledger::commit(
            &rig.store(),
            Entry {
                conv: conv.clone(),
                local: local.into(),
                kind: Kind::Message,
                target: None,
                outcome,
            },
            NOW,
        )
        .await
        .expect("commit");
    }
    let acked = rig.spawn(text(&conv, LOCAL)).await.expect("ends");
    assert_eq!(
        acked,
        TransmitOutcome::Sent {
            server_at: Some(1_761_500_009_000),
            per_recipient: Vec::new(),
        }
    );
    let refused = rig.spawn(text(&conv, OTHER_LOCAL)).await.expect("ends");
    assert_eq!(failed_kind(&refused), ErrorKind::ReachOutRefused);
    assert!(
        rig.session.sent().is_empty(),
        "a recorded outcome must write no frame"
    );
}

/// Fix F13 (robustness LOW-2): the ledger is read by the local alone, so a resend whose chat the core re-keyed (the
/// `pn:` form became the `lid:` form between the send and the resend) is still answered from the ledger with no
/// frame, a 463 is not met twice, and an edit finds its original's ack time. Mutant: the look by `(conv, local)`.
#[tokio::test]
async fn a_resend_under_another_form_of_the_chat_is_answered_from_the_ledger() {
    const PEER_LID: &str = "77700151839206";
    let rig = Rig::new().await;
    let number = dm(PEER_PN);
    let lid = ConvKey::Direct(user("lid", PEER_LID));
    rig.store()
        .write("learn", |tx| {
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 1, 1, 'test')",
                kourier_db::rusqlite::params![PEER_LID, PEER_PN],
            )
            .map(drop)
            .map_err(crate::error::sql("learn"))
        })
        .await
        .expect("learn");
    for (local, outcome) in [
        (
            LOCAL,
            Outcome::Acked {
                server_t: Some(1_761_500_009),
            },
        ),
        (OTHER_LOCAL, Outcome::Refused),
    ] {
        ledger::commit(
            &rig.store(),
            Entry {
                conv: number.clone(),
                local: local.into(),
                kind: Kind::Message,
                target: None,
                outcome,
            },
            NOW,
        )
        .await
        .expect("commit");
    }
    let acked = rig.spawn(text(&lid, LOCAL)).await.expect("ends");
    assert_eq!(
        acked,
        TransmitOutcome::Sent {
            server_at: Some(1_761_500_009_000),
            per_recipient: Vec::new(),
        },
        "the resend under the LID form is not answered from the ledger"
    );
    let refused = rig.spawn(text(&lid, OTHER_LOCAL)).await.expect("ends");
    assert_eq!(failed_kind(&refused), ErrorKind::ReachOutRefused);
    // An edit asked for under the LID form finds its original's time, recorded under the number form.
    let original = "3EB0F1A6C3E92B074419";
    rig.sent_before(&number, original, 1150).await;
    let expired = rig
        .spawn(edit(&lid, "3EB0C93D5A81E2F70614", original, 1_000))
        .await
        .expect("ends");
    assert_eq!(failed_kind(&expired), ErrorKind::Expired);
    assert!(
        rig.session.sent().is_empty(),
        "a recorded outcome or an expired edit must write no frame"
    );
}

/// D7 (critic B1): a direct chat the phone never started gets NO frame and fails `Unsupported` with the words
/// "start this chat on your phone" (never `ReachOutRefused`); a group is always started. Mutants: the gate removed;
/// the refusal said as `ReachOutRefused`.
#[tokio::test]
async fn a_chat_the_phone_never_started_gets_no_frame_and_a_group_is_always_started() {
    let rig = Rig::new().await;
    let stranger = dm(STRANGER_PN);
    let outcome = rig.spawn(text(&stranger, LOCAL)).await.expect("ends");
    let TransmitOutcome::Failed(error) = &outcome else {
        panic!("a send into a chat the phone never started must fail: {outcome:?}");
    };
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(
        error.detail.contains(START_ON_PHONE),
        "the refusal says where to start the chat"
    );
    assert!(
        rig.session.sent().is_empty(),
        "no frame into a chat the phone never started (D6)"
    );
    let attempt = rig.spawn(text(&group(), OTHER_LOCAL));
    rig.frames(1).await;
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// D5: a second Transmit of a local whose frame is in the air on the live connection writes nothing and hears that
/// frame's answer; once acked, a third is answered from the ledger. Mutant: the registry's `first` ignored.
#[tokio::test]
async fn a_second_transmit_of_a_local_in_the_air_writes_nothing_and_hears_the_first_answer() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let first = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    let second = rig.spawn(text(&conv, LOCAL));
    // The second registers its wait and writes nothing: give it the time to (wrongly) write.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        rig.session.sent().len(),
        1,
        "one frame per local per connection (D5)"
    );
    rig.ack(LOCAL, Answer::Acked { server_t: Some(7) });
    for attempt in [first, second] {
        assert!(matches!(
            attempt.await.expect("ends"),
            TransmitOutcome::Sent { .. }
        ));
    }
    assert!(matches!(
        rig.spawn(text(&conv, LOCAL)).await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
    assert_eq!(rig.session.sent().len(), 1, "no frame after acked");
}

/// T2 F1: the ONE write point (`write_once`) says each frame it wrote and how that frame was answered, in two INFO
/// lines of fixed words with a kind and a number and nothing else (no id, no local, no chat, no text): `a send frame
/// was written kind=message` after the write, and `a send frame was answered outcome=<acked|refused|nacked|no-ack>
/// code=<n>` when its wait ends. The run tool counts them (a 429 or a missing ack leaves the core's own retry
/// uncounted otherwise). A second Transmit of a local in the air, a remembered answer and a failure before the wire say
/// nothing. Mutants: the `written` line dropped; the `answered` line dropped; a 429 reported as `acked`; the line
/// printed for a Transmit that wrote no frame.
#[tokio::test]
async fn the_write_point_says_what_it_wrote_and_how_it_was_answered_in_fixed_words() {
    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let said = |needle: &str| lines.containing(needle);
    // An ack: one `written` line when the frame is out, one `answered` line when the ack came.
    let first = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    // The second Transmit of the local in the air writes no frame and says nothing.
    let second = rig.spawn(text(&conv, LOCAL));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        said("a send frame was written").len(),
        1,
        "{:?}",
        lines.all()
    );
    assert!(
        said("a send frame was answered").is_empty(),
        "{:?}",
        lines.all()
    );
    rig.ack(LOCAL, Answer::Acked { server_t: Some(7) });
    for attempt in [first, second] {
        assert!(matches!(
            attempt.await.expect("ends"),
            TransmitOutcome::Sent { .. }
        ));
    }
    let written = said("a send frame was written");
    let answered = said("a send frame was answered");
    assert_eq!(written.len(), 1, "{:?}", lines.all());
    assert_eq!(answered.len(), 1, "{:?}", lines.all());
    assert!(written[0].contains("kind=message"), "{written:?}");
    assert!(
        answered[0].contains("outcome=acked")
            && answered[0].contains("code=0")
            && answered[0].contains("again=no"),
        "{answered:?}"
    );
    // A 429: a nack with its code (the core retries it by itself after its floor: the run tool must see this line).
    let attempt = rig.spawn(text(&conv, OTHER_LOCAL));
    rig.frames(2).await;
    rig.ack(OTHER_LOCAL, Answer::Nacked(Nack::Code(429)));
    attempt.await.expect("ends");
    let answered = said("a send frame was answered");
    assert_eq!(answered.len(), 2, "{:?}", lines.all());
    assert!(
        answered[1].contains("outcome=nacked")
            && answered[1].contains("code=429")
            && answered[1].contains("again=300"),
        "{answered:?}"
    );
    // The core's retry inside the floor writes nothing and says nothing: the nack was already told.
    rig.spawn(text(&conv, OTHER_LOCAL)).await.expect("ends");
    assert_eq!(rig.session.sent().len(), 2);
    assert_eq!(
        said("a send frame was written").len(),
        2,
        "{:?}",
        lines.all()
    );
    assert_eq!(
        said("a send frame was answered").len(),
        2,
        "{:?}",
        lines.all()
    );
    // A 463 is a refusal, with its code.
    let attempt = rig.spawn(text(&conv, ORIGINAL));
    rig.frames(3).await;
    rig.ack(ORIGINAL, Answer::Nacked(Nack::Code(463)));
    attempt.await.expect("ends");
    let answered = said("a send frame was answered");
    assert!(
        answered[2].contains("outcome=refused")
            && answered[2].contains("code=463")
            && answered[2].contains("again=no"),
        "{answered:?}"
    );
    // No ack within ACK_WAIT: the frame is `no-ack`.
    let third = "3EB0C4E9A7152D8B6F30";
    let attempt = rig.spawn(text(&conv, third));
    rig.frames(4).await;
    tokio::time::timeout(ACK_WAIT + Duration::from_secs(5), attempt)
        .await
        .expect("the attempt answers after ACK_WAIT")
        .expect("ends");
    let answered = said("a send frame was answered");
    assert_eq!(answered.len(), 4, "{:?}", lines.all());
    assert!(
        answered[3].contains("outcome=no-ack")
            && answered[3].contains("code=0")
            && answered[3].contains("again=0"),
        "{answered:?}"
    );
    // Only the fixed words, one kind or one outcome and a code: no id, no chat, no text.
    for line in lines.all() {
        let counts = if line.contains("a send frame was written") {
            2
        } else if line.contains("a send frame was answered") {
            4
        } else {
            continue;
        };
        assert_eq!(line.matches('=').count(), counts, "{line}");
        for secret in [LOCAL, OTHER_LOCAL, ORIGINAL, third, PEER_PN, "synthetic"] {
            assert!(!line.contains(secret), "{line}");
        }
    }
    // A socket that failed WHILE writing counts as written (the frame may have left): the first line is said, and the
    // second says the connection ended.
    let socket = "3EB0A9B8C7D6E5F40312";
    rig.session.script_send(Err(SendFailure::Socket));
    let attempt = rig.spawn(text(&conv, socket));
    rig.frames(5).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        said("a send frame was written").len(),
        5,
        "{:?}",
        lines.all()
    );
    rig.acks.connection_ended();
    attempt.await.expect("ends");
    let answered = said("a send frame was answered");
    assert_eq!(answered.len(), 5, "{:?}", lines.all());
    assert!(
        answered[4].contains("outcome=connection-ended")
            && answered[4].contains("code=0")
            && answered[4].contains("again=0"),
        "{answered:?}"
    );
    // A failure before the wire writes no frame and says nothing.
    let before = lines.all().len();
    rig.session.script_send(Err(SendFailure::NotConnected));
    rig.spawn(text(&conv, "3EB0F1D2A3B4C5D6E7F8"))
        .await
        .expect("ends");
    assert_eq!(
        lines
            .all()
            .iter()
            .skip(before)
            .filter(|l| l.contains("a send frame"))
            .count(),
        0,
        "a frame that never left was counted: {:?}",
        lines.all()
    );
}

/// T2 G1 (NEW-1): the `again` field of the answered line is the core's own rule read off the very error the attempt
/// answers the core with (no table of codes of its own): for every code of `errors::by_code`'s arms, three codes that fall
/// to its default (0, 1, 499, 600) and an unreadable code, a nack says `again=no` EXACTLY when the error it gave the core
/// is not retryable, else the seconds of its `retry_after` (429 300, 421/500/503 30). The table is also written out, so a
/// change of the error and of the line together still fails. Mutants: `again_of` says `no` for a retryable kind (a 429
/// said `no`); `again_of` says the wait for a final kind (a 463 said 0).
#[tokio::test]
async fn the_answered_line_says_again_exactly_when_the_core_sends_the_local_again() {
    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let table: [(Option<u16>, &str); 14] = [
        (Some(463), "no"),
        (Some(475), "no"),
        (Some(429), "300"),
        (Some(421), "30"),
        (Some(500), "30"),
        (Some(503), "30"),
        (Some(404), "no"),
        (Some(400), "no"),
        (Some(479), "no"),
        (Some(0), "no"),
        (Some(1), "no"),
        (Some(499), "no"),
        (Some(600), "no"),
        (None, "no"),
    ];
    for (i, (code, expected)) in table.iter().enumerate() {
        let local = format!("3EB0C0DE{i:012X}");
        let attempt = rig.spawn(text(&conv, &local));
        rig.frames(i + 1).await;
        let nack = code.map_or(Nack::Unreadable, Nack::Code);
        rig.ack(&local, Answer::Nacked(nack));
        let TransmitOutcome::Failed(error) = attempt.await.expect("ends") else {
            panic!("a nack of {code:?} did not fail the send");
        };
        let answered = lines.containing("a send frame was answered");
        assert_eq!(answered.len(), i + 1, "{:?}", lines.all());
        let line = answered.last().expect("a line");
        let again = line
            .split("again=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_else(|| panic!("no again in {line}"));
        // The core's rule on the error the attempt gave it.
        let by_the_error = if error.is_retryable() {
            error
                .kind
                .retry_after()
                .map_or(0, |wait| wait.as_secs())
                .to_string()
        } else {
            "no".to_owned()
        };
        assert_eq!(again, by_the_error, "{code:?}: {line} vs {error:?}");
        assert_eq!(again, *expected, "{code:?}: {line}");
    }
}

/// T2 G1: a TIMER frame is answered to a command, not to the outbox: the core never sends it again, so even a
/// retryable error says `again=no`; every other kind of frame says what the error says. Mutant: the `timer` exception
/// removed.
#[test]
fn a_timer_frame_is_never_sent_again_by_the_core() {
    let rate_limited = Written::Waited(Waited::Answered(Answer::Nacked(Nack::Code(429))), None);
    let left = Written::Waited(
        Waited::Answered(Answer::Nacked(Nack::Code(429))),
        Some(Duration::from_millis(1500)),
    );
    let ended = Written::Waited(Waited::Ended, None);
    assert_eq!(
        super::answered_as(&rate_limited, "message"),
        ("nacked", 429, super::Again::After(Duration::from_secs(300)))
    );
    assert_eq!(
        super::answered_as(&rate_limited, "timer"),
        ("nacked", 429, super::Again::No)
    );
    // A remembered nack says what is left of its floor, rounded up in the line.
    let (_, _, again) = super::answered_as(&left, "message");
    assert_eq!(again, super::Again::After(Duration::from_millis(1500)));
    assert_eq!(again.to_string(), "2");
    assert_eq!(
        super::Again::After(Duration::from_secs(30)).to_string(),
        "30"
    );
    assert_eq!(
        super::answered_as(&ended, "timer"),
        ("connection-ended", 0, super::Again::No)
    );
    assert_eq!(
        super::answered_as(&ended, "message"),
        ("connection-ended", 0, super::Again::After(Duration::ZERO))
    );
}

/// T2 F1: the kind word of the `written` line, one per kind of request (a revoke is a delete for everyone; the
/// timer set passes "timer" itself). Mutant: a reaction reported as a message.
#[test]
fn the_kind_word_of_a_frame_names_what_kind_of_request_it_is() {
    let conv = dm(PEER_PN);
    let target = || MsgTarget {
        conv: Some(conv.clone()),
        author: TargetAuthor::Own,
        local: ORIGINAL.into(),
        alternates: Vec::new(),
    };
    let word = |kind: &OutgoingKind| super::frame_kind(kind);
    assert_eq!(word(&text(&conv, LOCAL).kind), "message");
    assert_eq!(
        word(&OutgoingKind::React {
            target: target(),
            emoji: "\u{1F44D}".into(),
            remove: false
        }),
        "reaction"
    );
    assert_eq!(word(&edit(&conv, LOCAL, ORIGINAL, 1).kind), "edit");
    assert_eq!(
        word(&OutgoingKind::Delete {
            target: target(),
            for_everyone: true
        }),
        "revoke"
    );
    assert_eq!(
        word(&OutgoingKind::Delete {
            target: target(),
            for_everyone: false
        }),
        "other"
    );
}

/// D6: a 463 is `ReachOutRefused`, recorded, and never sent again; any other nack fails by the table and is NOT
/// recorded in the ledger (but remembered by the registry for its floor: F1). Mutants: the 463 mapped to Transient;
/// every nack recorded.
#[tokio::test]
async fn a_463_is_refused_for_good_and_another_nack_is_not_recorded() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    rig.ack(LOCAL, Answer::Nacked(Nack::Code(463)));
    assert_eq!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::ReachOutRefused
    );
    assert_eq!(rig.looked(&conv, LOCAL).await, Some(Outcome::Refused));
    assert_eq!(
        failed_kind(&rig.spawn(text(&conv, LOCAL)).await.expect("ends")),
        ErrorKind::ReachOutRefused
    );
    assert_eq!(rig.session.sent().len(), 1, "a 463 is never sent again");

    let attempt = rig.spawn(text(&conv, OTHER_LOCAL));
    rig.frames(2).await;
    rig.ack(OTHER_LOCAL, Answer::Nacked(Nack::Code(503)));
    assert_eq!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::Transient {
            retry_after: Some(TRANSIENT_FLOOR)
        },
        "a retryable nack tells the core its floor (F1)"
    );
    assert_eq!(
        rig.looked(&conv, OTHER_LOCAL).await,
        None,
        "a nack other than 463 is not recorded (critic H2)"
    );
}

/// Fix F8 (interop LOW): a 475 (upstream's `NewChatMessagesCapped`) is met like a 463 until a real run shows what it
/// means: `ReachOutRefused`, recorded `refused`, and a resend writes NO frame. Mutants: 475 not recorded; 475 not
/// mapped to `ReachOutRefused`.
#[tokio::test]
async fn a_475_is_refused_for_good_like_a_463() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    rig.ack(LOCAL, Answer::Nacked(Nack::Code(475)));
    assert_eq!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::ReachOutRefused
    );
    assert_eq!(rig.looked(&conv, LOCAL).await, Some(Outcome::Refused));
    assert_eq!(
        failed_kind(&rig.spawn(text(&conv, LOCAL)).await.expect("ends")),
        ErrorKind::ReachOutRefused,
        "the resend is answered from the ledger"
    );
    assert_eq!(rig.session.sent().len(), 1, "a 475 is never sent again");
}

/// What a remembered nack's retryable failure says is left of its floor.
fn left_of(kind: &ErrorKind) -> Duration {
    let (ErrorKind::Transient { retry_after } | ErrorKind::RateLimited { retry_after }) = kind
    else {
        panic!("a retryable failure was expected: {kind:?}");
    };
    retry_after.expect("a remembered nack says what is left of its floor")
}

/// `left` is a little less than `floor` (some time passed since the nack), and more than `floor - slack`.
fn near(left: Duration, floor: Duration, slack: Duration) {
    assert!(
        left <= floor && left > floor - slack,
        "left {left:?} of {floor:?}"
    );
}

/// Fix F1 (the interop BLOCKER; amends D21): after a retryable nack the core's own retry of the same local, at
/// once on the same connection, writes NO second frame: the answer is the remembered failure with the time left of
/// its floor (a 429 five minutes, a 500, 503 and 421 thirty seconds); after the floor ONE new frame goes out, and
/// its own nack starts a floor again. Mutants: the nack not remembered (a burst of frames under a rate limit);
/// the floor 0.
#[tokio::test]
async fn a_retryable_nack_is_not_answered_by_a_second_frame_inside_its_floor() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    // A 429: the live answer carries the whole floor; the retry at once writes nothing.
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    rig.ack(LOCAL, Answer::Nacked(Nack::Code(429)));
    assert_eq!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::RateLimited {
            retry_after: Some(RATE_LIMITED_FLOOR)
        }
    );
    let retry = failed_kind(&rig.spawn(text(&conv, LOCAL)).await.expect("ends"));
    assert!(matches!(retry, ErrorKind::RateLimited { .. }), "{retry:?}");
    near(left_of(&retry), RATE_LIMITED_FLOOR, Duration::from_secs(30));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rig.session.sent().len(),
        1,
        "a 429 is not answered by a second frame of the local on the connection"
    );
    rig.acks
        .advance(RATE_LIMITED_FLOOR - Duration::from_secs(60));
    let retry = failed_kind(&rig.spawn(text(&conv, LOCAL)).await.expect("ends"));
    near(
        left_of(&retry),
        Duration::from_secs(60),
        Duration::from_secs(30),
    );
    assert_eq!(rig.session.sent().len(), 1, "a minute before the end");
    // After the floor ONE frame goes out; it is the local's second and last one while it is in the air.
    rig.acks.advance(Duration::from_secs(60));
    let again = rig.spawn(text(&conv, LOCAL));
    rig.frames(2).await;
    let third = rig.spawn(text(&conv, LOCAL));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.session.sent().len(), 2, "one new frame per floor");
    rig.ack(LOCAL, Answer::Acked { server_t: Some(5) });
    for attempt in [again, third] {
        assert!(matches!(
            attempt.await.expect("ends"),
            TransmitOutcome::Sent { .. }
        ));
    }

    // A 500: thirty seconds.
    let attempt = rig.spawn(text(&conv, OTHER_LOCAL));
    rig.frames(3).await;
    rig.ack(OTHER_LOCAL, Answer::Nacked(Nack::Code(500)));
    attempt.await.expect("ends");
    let retry = failed_kind(&rig.spawn(text(&conv, OTHER_LOCAL)).await.expect("ends"));
    assert!(matches!(retry, ErrorKind::Transient { .. }), "{retry:?}");
    near(left_of(&retry), TRANSIENT_FLOOR, Duration::from_secs(10));
    assert_eq!(
        rig.session.sent().len(),
        3,
        "a 500 is not written again at once"
    );
    rig.acks.advance(TRANSIENT_FLOOR);
    let attempt = rig.spawn(text(&conv, OTHER_LOCAL));
    rig.frames(4).await;
    rig.ack(OTHER_LOCAL, Answer::Nacked(Nack::Code(503)));
    assert!(matches!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::Transient { .. }
    ));
    let retry = failed_kind(&rig.spawn(text(&conv, OTHER_LOCAL)).await.expect("ends"));
    near(left_of(&retry), TRANSIENT_FLOOR, Duration::from_secs(10));
    assert_eq!(
        rig.session.sent().len(),
        4,
        "the second nack starts a floor again"
    );
    assert!(
        rig.session
            .sent()
            .iter()
            .all(|f| f.groups == Groups::Cached),
        "only a 421 in a group asks for fresh group metadata"
    );
}

/// Fix F1: a nack without a floor (the server refused the frame itself) is answered again with no frame for the
/// rest of the connection, and the next connection writes it. Mutant: such a nack forgotten.
#[tokio::test]
async fn a_nack_without_a_floor_is_answered_without_a_frame_until_the_connection_ends() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    rig.ack(LOCAL, Answer::Nacked(Nack::Code(400)));
    assert_eq!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::InvalidInput
    );
    rig.acks.advance(Duration::from_secs(86_400));
    assert_eq!(
        failed_kind(&rig.spawn(text(&conv, LOCAL)).await.expect("ends")),
        ErrorKind::InvalidInput,
        "Send Again hears the same refusal"
    );
    assert_eq!(
        rig.session.sent().len(),
        1,
        "no second frame on the connection"
    );
    rig.acks.connection_ended();
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(2).await;
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    attempt.await.expect("ends");
}

/// Fix F1: a 421 (the group's addressing was stale) is held for its floor like a 500, and the first frame after it
/// asks for fresh group metadata; a frame in a direct chat never does. Mutant: the refresh not asked.
#[tokio::test]
async fn the_frame_after_a_421_asks_for_fresh_group_metadata() {
    let rig = Rig::new().await;
    let conv = group();
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    assert_eq!(rig.session.sent()[0].groups, Groups::Cached);
    rig.ack(LOCAL, Answer::Nacked(Nack::Code(421)));
    assert_eq!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::Transient {
            retry_after: Some(TRANSIENT_FLOOR)
        }
    );
    let held = failed_kind(&rig.spawn(text(&conv, LOCAL)).await.expect("ends"));
    near(left_of(&held), TRANSIENT_FLOOR, Duration::from_secs(10));
    assert_eq!(rig.session.sent().len(), 1, "the floor holds");
    rig.acks.advance(TRANSIENT_FLOOR);
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(2).await;
    assert_eq!(
        rig.session.sent()[1].groups,
        Groups::Refresh,
        "the frame after a 421 asks upstream for fresh group metadata"
    );
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    attempt.await.expect("ends");
}

/// Fix F3 (the recycled number): a number that got a new LID lends the older LID's start to nobody. The older LID
/// started the chat; the Transmit into the number (written to its CURRENT LID) and into the newer LID is refused with
/// no frame, the one into the older LID goes out. Mutant: `holds` looks at every LID of the number.
#[tokio::test]
async fn a_transmit_to_another_lid_of_a_number_is_not_a_start() {
    const RECYCLED_PN: &str = "15550184739206";
    const OLD_LID: &str = "77700128459036";
    const NEW_LID: &str = "77700139570147";
    let rig = Rig::new().await;
    rig.store()
        .write("learn", |tx| {
            for (lid, at) in [(OLD_LID, 1), (NEW_LID, 2)] {
                tx.execute(
                    "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                     VALUES (?1, ?2, ?3, ?3, 'test')",
                    kourier_db::rusqlite::params![lid, RECYCLED_PN, at],
                )
                .map_err(crate::error::sql("learn"))?;
            }
            started::note(
                tx,
                &ConvKey::Direct(user("lid", OLD_LID)),
                How::Peer,
            )
            .map(drop)
        })
        .await
        .expect("learn");
    for conv in [
        ConvKey::Direct(user("lid", NEW_LID)),
        ConvKey::Direct(user("pn", RECYCLED_PN)),
    ] {
        let outcome = rig.spawn(text(&conv, LOCAL)).await.expect("ends");
        assert!(
            matches!(&outcome, TransmitOutcome::Failed(e) if e.detail == START_ON_PHONE),
            "the start of another LID of the number is no start: {outcome:?}"
        );
    }
    assert!(rig.session.sent().is_empty(), "no frame went to a stranger");
    let attempt = rig.spawn(text(&ConvKey::Direct(user("lid", OLD_LID)), OTHER_LOCAL));
    rig.frames(1).await;
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// brief §2.3: a failure before the wire writes nothing and leaves no wait (the next attempt writes); a recipient
/// without a usable device is tried ONCE more with the list asked again; a socket that failed mid-write keeps its
/// wait until the connection ends (transient). Mutants: the retry without `Refresh`; the withdrawal skipped.
#[tokio::test]
async fn a_failure_before_the_wire_leaves_no_wait_and_a_missing_device_is_tried_once_with_a_fresh_list()
 {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    rig.session.script_send(Err(SendFailure::NoRecipientDevice));
    rig.session.script_send(Err(SendFailure::NoRecipientDevice));
    let outcome = rig.spawn(text(&conv, LOCAL)).await.expect("ends");
    assert_eq!(failed_kind(&outcome), ErrorKind::PeerUnregistered);
    let devices: Vec<Devices> = rig.session.sent().iter().map(|f| f.devices).collect();
    assert_eq!(
        devices,
        [Devices::Cached, Devices::Refresh],
        "one retry, with the device list asked again"
    );

    rig.session.script_send(Err(SendFailure::NotConnected));
    assert_eq!(
        failed_kind(&rig.spawn(text(&conv, OTHER_LOCAL)).await.expect("ends")),
        ErrorKind::Transient { retry_after: None }
    );
    let attempt = rig.spawn(text(&conv, OTHER_LOCAL));
    rig.frames(4).await;
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    assert!(
        matches!(attempt.await.expect("ends"), TransmitOutcome::Sent { .. }),
        "a frame that never left leaves no wait behind"
    );

    rig.session.script_send(Err(SendFailure::Socket));
    let third = "3EB0C8D05E2F71A49316";
    let attempt = rig.spawn(text(&conv, third));
    rig.frames(5).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !attempt.is_finished(),
        "a frame that may have left waits for its ack"
    );
    rig.acks.connection_ended();
    assert_eq!(
        failed_kind(&attempt.await.expect("ends")),
        ErrorKind::Transient { retry_after: None }
    );
}

/// The quiet cases: a delete for me makes no frame and is answered `Sent` (D9); our own ids not known yet (critic
/// H4) and no client make no frame and are transient. Mutant: the own-id check removed.
#[tokio::test]
async fn a_delete_for_me_and_a_send_before_our_ids_are_known_write_nothing() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let for_me = req(
        &conv,
        LOCAL,
        OutgoingKind::Delete {
            target: MsgTarget {
                conv: Some(conv.clone()),
                author: TargetAuthor::Own,
                local: OTHER_LOCAL.into(),
                alternates: Vec::new(),
            },
            for_everyone: false,
        },
    );
    assert_eq!(
        rig.spawn(for_me).await.expect("ends"),
        TransmitOutcome::Sent {
            server_at: None,
            per_recipient: Vec::new(),
        }
    );
    let mut parts = rig.parts();
    parts.own = OwnIds {
        lid: Some(user("lid", OWN_LID)),
        pn: None,
    };
    assert_eq!(
        failed_kind(&spawn_with(parts, text(&conv, LOCAL)).await.expect("ends")),
        ErrorKind::Transient { retry_after: None }
    );
    let mut parts = rig.parts();
    parts.session = None;
    assert_eq!(
        failed_kind(&spawn_with(parts, text(&conv, LOCAL)).await.expect("ends")),
        ErrorKind::Transient { retry_after: None }
    );
    assert!(rig.session.sent().is_empty());
}

/// D5 (critic H1): a frame that hears no ack within `ACK_WAIT` is answered transient and its connection is ENDED
/// (the session drops it, the registry begins the next one); a resend while the end is under way writes nothing and
/// ends with the connection; the resend after it goes out on the next connection with the SAME id. Mutants: the
/// connection not dropped; the registry's end skipped.
#[tokio::test]
async fn a_lost_ack_ends_the_connection_and_the_resend_goes_out_on_the_next_with_the_same_id() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let gate = rig.session.hold_drops();
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    let outcome = tokio::time::timeout(ACK_WAIT + Duration::from_secs(5), attempt)
        .await
        .expect("the attempt answers after ACK_WAIT")
        .expect("ends");
    assert_eq!(
        failed_kind(&outcome),
        ErrorKind::Transient { retry_after: None }
    );
    rig.drops(1).await;
    // The core's resend comes at once, while the socket is still closing: no second frame on that connection. The
    // attempt reads the store before it joins the wait, and how long that takes is the machine's: the test WAITS for
    // the resend to have joined the wait of the ending connection (two waits of the local) before it lets the socket
    // go. A fixed sleep here let a loaded machine open the gate first; the resend then joined the NEXT connection as
    // its first frame, waited its own `ACK_WAIT` and ended that connection too (a second drop; W6a1 A8, strike 1).
    let during = rig.spawn(text(&conv, LOCAL));
    rig.waits(LOCAL, 2).await;
    assert_eq!(
        rig.session.sent().len(),
        1,
        "no second frame of a local on one connection (D5)"
    );
    gate.notify_one();
    assert_eq!(
        failed_kind(&during.await.expect("ends")),
        ErrorKind::Transient { retry_after: None },
        "a wait of the ended connection answers transient"
    );
    assert_eq!(rig.acks.connection(), 1, "the registry's connection ended");
    assert_eq!(
        rig.session.count(&Call::DropConnection),
        1,
        "one end per connection"
    );
    let resend = rig.spawn(text(&conv, LOCAL));
    rig.frames(2).await;
    assert_eq!(
        rig.session.sent()[1].id,
        LOCAL,
        "the resend keeps the id (his answer)"
    );
    rig.ack(LOCAL, Answer::Acked { server_t: Some(11) });
    assert!(matches!(
        resend.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// D5: an ack that comes after the transient answer, before the connection's end, is still recorded in the ledger
/// (the attempt listens on after its reply), so the core's resend is answered `Sent` from the ledger with NO frame.
/// Mutant: the late ack not recorded.
#[tokio::test]
async fn a_late_ack_is_recorded_and_the_resend_is_answered_from_the_ledger() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let gate = rig.session.hold_drops();
    let attempt = rig.spawn(text(&conv, LOCAL));
    rig.frames(1).await;
    let outcome = tokio::time::timeout(ACK_WAIT + Duration::from_secs(5), attempt)
        .await
        .expect("the attempt answers after ACK_WAIT")
        .expect("ends");
    assert_eq!(
        failed_kind(&outcome),
        ErrorKind::Transient { retry_after: None }
    );
    rig.drops(1).await;
    rig.ack(
        LOCAL,
        Answer::Acked {
            server_t: Some(1_761_500_013),
        },
    );
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while rig.looked(&conv, LOCAL).await.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the late ack must reach the ledger");
    assert_eq!(
        rig.spawn(text(&conv, LOCAL)).await.expect("ends"),
        TransmitOutcome::Sent {
            server_at: Some(1_761_500_013_000),
            per_recipient: Vec::new(),
        }
    );
    assert_eq!(rig.session.sent().len(), 1, "no frame after a late ack");
}

/// An edit of the own message `original` in `conv` with a new text, made `waited_ms` before `NOW`.
fn edit(conv: &ConvKey, local: &str, original: &str, waited_ms: i64) -> OutgoingRequest {
    OutgoingRequest {
        sent_at: NOW - waited_ms,
        ..req(
            conv,
            local,
            OutgoingKind::Edit {
                target: MsgTarget {
                    conv: Some(conv.clone()),
                    author: TargetAuthor::Own,
                    local: original.into(),
                    alternates: Vec::new(),
                },
                body: Body::Text(Text::plain("synthetic edited hello")),
            },
        )
    }
}

impl Rig {
    /// The ledger says Kourier sent `local` in `conv` and the server stamped it `age_secs` before `NOW`.
    async fn sent_before(&self, conv: &ConvKey, local: &str, age_secs: i64) {
        ledger::commit(
            &self.store(),
            Entry {
                conv: conv.clone(),
                local: local.into(),
                kind: Kind::Message,
                target: None,
                outcome: Outcome::Acked {
                    server_t: Some(NOW / 1000 - age_secs),
                },
            },
            NOW,
        )
        .await
        .expect("commit");
    }
}

/// D9 (critic M3): the slack rule judges the ORIGINAL's age when the ledger knows it, not the edit's wait: an
/// edit made early that waited 400 s (offline, a hold) is still sent while its original is 460 s old. Mutant: the
/// proxy judges every edit.
#[tokio::test]
async fn an_early_edit_that_waited_long_is_sent_while_its_original_is_young() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let original = "3EB0E4F27A9C61D58832";
    rig.sent_before(&conv, original, 460).await;
    let attempt = rig.spawn(edit(&conv, LOCAL, original, 400_000));
    rig.frames(1).await;
    assert_eq!(rig.session.sent()[0].id, LOCAL, "the edit is written");
    rig.ack(
        LOCAL,
        Answer::Acked {
            server_t: Some(NOW / 1000),
        },
    );
    assert!(
        matches!(attempt.await.expect("ends"), TransmitOutcome::Sent { .. }),
        "an edit of a young original is sent whatever it waited"
    );
}

/// D9 (critic M3; brief MUTANT "slack 1200 s"): an edit whose original Kourier sent 1,150 s ago is `Expired` with
/// NO frame (the receivers take an edit up to 1200 s after its original; the limit leaves room for the clocks),
/// though the edit itself was made only a second ago. Mutants: the limit at the receivers' 1200 s; the rule removed.
#[tokio::test]
async fn an_edit_whose_original_is_too_old_is_expired_and_writes_nothing() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let original = "3EB0F1A6C3E92B074419";
    rig.sent_before(&conv, original, 1150).await;
    let outcome = rig
        .spawn(edit(&conv, LOCAL, original, 1_000))
        .await
        .expect("ends");
    assert_eq!(failed_kind(&outcome), ErrorKind::Expired);
    assert!(
        rig.session.sent().is_empty(),
        "an edit past the receivers' window must write no frame"
    );
    assert!(
        rig.looked(&conv, LOCAL).await.is_none(),
        "an expired edit is not a recorded outcome"
    );
}

/// D9, fix F15: an edit of an original whose time the backend does not know (one sent from the phone) is judged by
/// the PROXY: waited 170 s -> sent; waited 190 s or 400 s (more than 180 s, the receivers' window less the stated
/// 900 s and the 120 s margin) -> `Expired`, no frame. A revoke has no such rule. Mutants: the proxy removed (an
/// unknown original never expires); the proxy back at 300 s (the 190 s edit is sent).
#[tokio::test]
async fn without_the_originals_time_the_proxy_judges_an_edit_by_its_wait() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let from_phone = "3EB09D2E5B7F48C16653";
    for waited_ms in [400_000, 190_000] {
        let late = rig
            .spawn(edit(&conv, LOCAL, from_phone, waited_ms))
            .await
            .expect("ends");
        assert_eq!(
            failed_kind(&late),
            ErrorKind::Expired,
            "an edit that waited {waited_ms} ms is past the proxy"
        );
    }
    assert!(
        rig.session.sent().is_empty(),
        "the late edit writes no frame"
    );
    let young = rig.spawn(edit(&conv, OTHER_LOCAL, from_phone, 170_000));
    rig.frames(1).await;
    let revoke = OutgoingRequest {
        sent_at: NOW - 4_000_000,
        ..req(
            &conv,
            "3EB0A7C35E1D96F20487",
            OutgoingKind::Delete {
                target: MsgTarget {
                    conv: Some(conv.clone()),
                    author: TargetAuthor::Own,
                    local: from_phone.into(),
                    alternates: Vec::new(),
                },
                for_everyone: true,
            },
        )
    };
    let revoke = rig.spawn(revoke);
    rig.frames(2).await;
    let ids: Vec<String> = rig.session.sent().iter().map(|f| f.id.clone()).collect();
    assert_eq!(
        ids,
        [OTHER_LOCAL.to_owned(), "3EB0A7C35E1D96F20487".to_owned()],
        "the young edit and the old revoke are written"
    );
    rig.acks.connection_ended();
    for attempt in [young, revoke] {
        assert_eq!(
            failed_kind(&attempt.await.expect("ends")),
            ErrorKind::Transient { retry_after: None }
        );
    }
}

/// D8: while the lock is unknown or a hold is known the attempt writes NO frame and answers
/// `Transient{retry_after: Some(HOLD_RETRY)}`; an outcome the ledger has is still the answer. Mutant: the gate removed.
#[tokio::test]
async fn a_send_is_held_back_while_the_lock_is_unknown_or_held() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    for gate in [Gate::Unknown, Gate::Held] {
        let parts = Parts {
            hold: Recheck::fixed(gate),
            ..rig.parts()
        };
        let outcome = spawn_with(parts, text(&conv, LOCAL)).await.expect("ends");
        assert_eq!(
            failed_kind(&outcome),
            ErrorKind::Transient {
                retry_after: Some(HOLD_RETRY)
            },
            "a held send waits"
        );
    }
    assert!(rig.session.sent().is_empty(), "no frame while held");
    ledger::commit(
        &rig.store(),
        Entry {
            conv: conv.clone(),
            local: OTHER_LOCAL.into(),
            kind: Kind::Message,
            target: None,
            outcome: Outcome::Acked { server_t: None },
        },
        NOW,
    )
    .await
    .expect("commit");
    let parts = Parts {
        hold: Recheck::fixed(Gate::Held),
        ..rig.parts()
    };
    let recorded = spawn_with(parts, text(&conv, OTHER_LOCAL))
        .await
        .expect("ends");
    assert!(matches!(recorded, TransmitOutcome::Sent { .. }));
    assert!(rig.session.sent().is_empty());
}

/// Fix F6 (privacy LOW): the gate is asked AGAIN right before the frame is written. A lock that becomes known while the
/// attempt awaits (here the account's LID mode, a slow ask) writes NO frame and answers `Transient{HOLD_RETRY}`; the
/// registry holds no wait of the local afterwards (a later send is a first one). Mutant: the second look removed.
#[tokio::test]
async fn a_lock_that_becomes_known_during_the_attempt_stops_its_frame() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let pushed = Arc::new(AtomicBool::new(false));
    let seen = pushed.clone();
    let parts = Parts {
        hold: Recheck::new(move || {
            if seen.load(Ordering::SeqCst) {
                Gate::Held
            } else {
                Gate::Open
            }
        }),
        ..rig.parts()
    };
    let slow = rig.session.hold_migrated();
    let attempt = spawn_with(parts, text(&conv, LOCAL));
    tokio::time::timeout(Duration::from_secs(10), async {
        while rig.session.count(&Call::LidMigrated) < 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the attempt reaches the account's mode");
    pushed.store(true, Ordering::SeqCst);
    slow.notify_one();
    let outcome = attempt.await.expect("ends");
    assert_eq!(
        failed_kind(&outcome),
        ErrorKind::Transient {
            retry_after: Some(HOLD_RETRY)
        },
        "a lock pushed during the attempt is not met"
    );
    assert!(
        rig.session.sent().is_empty(),
        "a frame was written into a lock the account already knew"
    );
    // The lock lifts: the same local is a first send again.
    pushed.store(false, Ordering::SeqCst);
    slow.notify_one();
    let parts = Parts {
        hold: Recheck::fixed(Gate::Open),
        ..rig.parts()
    };
    let again = spawn_with(parts, text(&conv, LOCAL));
    rig.frames(1).await;
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        again.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// D12 (critic M5): right after a link (this run's, 1 minute ago; no bootstrap delivered) a direct send with no stored
/// token writes NO frame and answers `Transient{retry_after: Some(TOKEN_RETRY)}`; once the chat has a token (here
/// under its LID, the form the history keys it by) it is written; and 31 minutes after the link it goes out bare.
/// Mutant: the gate removed from the attempt.
#[tokio::test]
async fn a_direct_send_waits_for_the_historys_token_right_after_a_link() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let parts = |linked: i64| Parts {
        linked_ms: Some(linked),
        ..rig.parts()
    };
    let held = spawn_with(parts(NOW - 60_000), text(&conv, LOCAL))
        .await
        .expect("ends");
    assert_eq!(
        failed_kind(&held),
        ErrorKind::Transient {
            retry_after: Some(TOKEN_RETRY)
        },
        "a send right after a link waits for the chat's token"
    );
    assert!(
        rig.session.sent().is_empty(),
        "no frame while the token may come"
    );
    // 31 minutes after the link: sent bare.
    let late = spawn_with(parts(NOW - 31 * 60_000), text(&conv, LOCAL));
    rig.frames(1).await;
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        late.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
    // A token under the chat's LID lets the next send go at once after the link.
    rig.store()
        .write("pair and token", |tx| {
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES ('77700136925814', ?1, 1, 1, 'other')",
                [PEER_PN],
            )
            .map_err(crate::error::sql("pair"))?;
            crate::store::keep_history_token(tx, "77700136925814", &[7, 7, 7], 1_761_400_000, None)
        })
        .await
        .expect("write");
    let tokened = spawn_with(parts(NOW - 60_000), text(&conv, OTHER_LOCAL));
    rig.frames(2).await;
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        tokened.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// A reaction in `conv` to the message `ORIGINAL` of the member the core holds by number.
fn reaction_to_a_member(conv: &ConvKey, local: &str) -> OutgoingRequest {
    req(
        conv,
        local,
        OutgoingKind::React {
            target: MsgTarget {
                conv: Some(conv.clone()),
                author: TargetAuthor::User(user("pn", AUTHOR_PN)),
                local: ORIGINAL.into(),
                alternates: Vec::new(),
            },
            emoji: "\u{1F44D}".into(),
            remove: false,
        },
    )
}

/// The `participant` of the key of the reaction `frame` carries.
fn reacted_participant(frame: &crate::session::Outgoing) -> Option<String> {
    frame
        .message
        .reaction_message
        .as_option()
        .and_then(|r| r.key.as_option())
        .and_then(|key| key.participant.clone())
}

/// Fix F20 (re-check N2, privacy): a reaction to the message of a member the core holds by NUMBER, in a group that
/// hides numbers, never writes the number as the key's `participant` (every member reads it). In a LID group whose
/// mapping lacks the author's LID the reaction fails for good (`Unsupported`, no retry), in a group whose mode could
/// not be read it is `Transient` (the mode is asked again at the next send), and neither writes a frame. A retry that
/// reads the mode as numbers writes the number (that group shows numbers anyway); once the mapping knows the LID the
/// LID goes out. Mutants: the key writes the number again; an unknown mode answered as a LID group; the LID group's
/// refusal answered as transient.
#[tokio::test]
async fn a_reaction_to_a_members_message_never_names_a_number_in_a_group_that_hides_numbers() {
    let rig = Rig::new().await;
    let conv = group();
    // A LID group and no LID known for the author: refused for good, no frame.
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let refused = rig
        .spawn(reaction_to_a_member(&conv, LOCAL))
        .await
        .expect("ends");
    assert_eq!(
        failed_kind(&refused),
        ErrorKind::Unsupported,
        "a LID group would read the number: no retry"
    );
    // A group whose mode could not be read: transient, no frame.
    rig.session.script_group_mode(Err(SendFailure::IqTimeout));
    let waiting = rig
        .spawn(reaction_to_a_member(&conv, LOCAL))
        .await
        .expect("ends");
    assert_eq!(
        failed_kind(&waiting),
        ErrorKind::Transient { retry_after: None },
        "an unknown mode is asked again at the next send"
    );
    assert!(
        rig.session.sent().is_empty(),
        "a frame went out although its key would have named the number"
    );
    // The next send reads the mode: a group addressed by numbers names numbers (the group shows them anyway).
    rig.session.script_group_mode(Ok(Namespace::Pn));
    let attempt = rig.spawn(reaction_to_a_member(&conv, LOCAL));
    rig.frames(1).await;
    assert_eq!(
        reacted_participant(&rig.session.sent()[0]),
        Some(format!("{AUTHOR_PN}@s.whatsapp.net")),
        "a group addressed by numbers names the author by his number"
    );
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
    // The mapping learns the author's LID: a LID group names the LID, and no digit of the number is in the frame.
    rig.store()
        .write("learn", |tx| {
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 1, 1, 'test')",
                kourier_db::rusqlite::params![AUTHOR_LID, AUTHOR_PN],
            )
            .map(drop)
            .map_err(crate::error::sql("learn"))
        })
        .await
        .expect("learn");
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let attempt = rig.spawn(reaction_to_a_member(&conv, OTHER_LOCAL));
    rig.frames(2).await;
    let frame = rig.session.sent()[1].clone();
    assert_eq!(
        reacted_participant(&frame),
        Some(format!("{AUTHOR_LID}@lid")),
        "a LID group names the author by his LID"
    );
    assert!(
        !format!("{:?}", frame.message).contains("95206837"),
        "the number is in the frame"
    );
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// A delete for everyone, in `conv`, of the message `ORIGINAL` of the member `AUTHOR_LID`.
fn admin_delete(conv: &ConvKey, local: &str) -> OutgoingRequest {
    req(
        conv,
        local,
        OutgoingKind::Delete {
            target: MsgTarget {
                conv: Some(conv.clone()),
                author: TargetAuthor::User(user("lid", AUTHOR_LID)),
                local: ORIGINAL.into(),
                alternates: Vec::new(),
            },
            for_everyone: true,
        },
    )
}

/// The group's row as the last fetch left it, with our place: `own_admin`, and whether we are still in the group.
async fn we_are_in_the_group_as(rig: &Rig, own_admin: bool, not_member: bool) {
    let conv = group();
    rig.store()
        .write("seed the group's row", move |tx| {
            crate::store::people::put_group(
                tx,
                &conv,
                &crate::groups::State {
                    members_stated: true,
                    rules: Some(crate::groups::Rules {
                        own_admin,
                        ..crate::groups::Rules::default()
                    }),
                    not_member,
                    ..crate::groups::State::default()
                },
            )
        })
        .await
        .expect("seed");
}

/// W6a2 B7b (his U4): the admin revoke of another member's message is written only when the group's ROW says we are an
/// admin there and still in it. No row (the group was never fetched), a row that says we are a member, and a row of a
/// group we left (`not_member`) all fail for good with no frame; with the row it is ONE frame whose key is the
/// member's (`fromMe = false`, the author as `participant`), its ack is recorded as a revoke of the original, and the
/// answer is `Sent`. The same request in a 1:1 chat never gets a frame. Mutants: the row not read (`admin` false
/// or true for ever); `not_member` ignored.
#[tokio::test]
async fn an_admin_revoke_is_written_only_when_the_groups_row_says_we_are_an_admin_still_in_it() {
    let rig = Rig::new().await;
    let conv = group();
    rig.session.script_group_mode(Ok(Namespace::Lid));
    // No row at all.
    let none = rig.spawn(admin_delete(&conv, LOCAL)).await.expect("ends");
    assert_eq!(
        failed_kind(&none),
        ErrorKind::Unsupported,
        "no row, no admin"
    );
    // A member.
    we_are_in_the_group_as(&rig, false, false).await;
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let member = rig.spawn(admin_delete(&conv, LOCAL)).await.expect("ends");
    assert_eq!(failed_kind(&member), ErrorKind::Unsupported, "a member");
    // An admin who has left.
    we_are_in_the_group_as(&rig, true, true).await;
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let left = rig.spawn(admin_delete(&conv, LOCAL)).await.expect("ends");
    assert_eq!(
        failed_kind(&left),
        ErrorKind::Unsupported,
        "an admin who left"
    );
    assert!(
        rig.session.sent().is_empty(),
        "a frame went out although we are no admin of the group"
    );
    // An admin still in the group.
    we_are_in_the_group_as(&rig, true, false).await;
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let attempt = rig.spawn(admin_delete(&conv, LOCAL));
    rig.frames(1).await;
    let frame = rig.session.sent()[0].clone();
    let protocol = frame
        .message
        .protocol_message
        .as_option()
        .expect("a protocol message");
    assert_eq!(
        protocol.r#type,
        Some(waproto::whatsapp::message::protocol_message::Type::Revoke)
    );
    let key = protocol.key.as_option().expect("the key");
    assert_eq!(key.from_me, Some(false));
    assert_eq!(
        key.participant.as_deref(),
        Some(format!("{AUTHOR_LID}@lid").as_str())
    );
    assert_eq!(
        frame.id, LOCAL,
        "the frame is the core's local, as every send"
    );
    rig.ack(
        LOCAL,
        Answer::Acked {
            server_t: Some(1_761_500_001),
        },
    );
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
    assert!(
        matches!(rig.looked(&conv, LOCAL).await, Some(Outcome::Acked { .. })),
        "the acked revoke is recorded"
    );
    // A 1:1 chat: the same request (the peer started the chat) writes nothing, admin row or not.
    let before = rig.session.sent().len();
    let direct = rig
        .spawn(admin_delete(&dm(PEER_PN), OTHER_LOCAL))
        .await
        .expect("ends");
    assert_eq!(
        failed_kind(&direct),
        ErrorKind::Unsupported,
        "no admin in a 1:1 chat"
    );
    assert_eq!(
        rig.session.sent().len(),
        before,
        "a frame went into a 1:1 chat"
    );
}

/// Fix F4 (PROTOCOL-7): the admin revoke goes into no group the row says is suspended, although we are an admin there:
/// failed for good, no frame. (A group's timer set is refused earlier still: `send::timer::request` takes a direct chat
/// only.)
/// Mutant: `is_admin` ignores `suspended`.
#[tokio::test]
async fn an_admin_revoke_goes_into_no_suspended_group() {
    let rig = Rig::new().await;
    let conv = group();
    rig.store()
        .write("seed a suspended group", {
            let conv = conv.clone();
            move |tx| {
                crate::store::people::put_group(
                    tx,
                    &conv,
                    &crate::groups::State {
                        members_stated: true,
                        rules: Some(crate::groups::Rules {
                            own_admin: true,
                            suspended: true,
                            ..crate::groups::Rules::default()
                        }),
                        ..crate::groups::State::default()
                    },
                )
            }
        })
        .await
        .expect("seed");
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let ended = rig.spawn(admin_delete(&conv, LOCAL)).await.expect("ends");
    assert_eq!(failed_kind(&ended), ErrorKind::Unsupported);
    assert!(
        rig.session.sent().is_empty(),
        "a frame went into a suspended group"
    );
}

/// Fix F3 (PROTOCOL-6): an admin's revoke names the author exactly as the original message's key did. The inbox ledger
/// remembers the form the message arrived under (here by number); the core holds the author by the other id of the same
/// person (a merge re-authored him) and the group is addressed by LIDs now, yet the revoke's `participant` is the number
/// the original key carried. A message the ledger has no row of (history, swept) is written in the group's mode as
/// before, and so is one whose id another member's ledger row shares (not his).
/// Mutants: the ledger form not read (`received_as` dropped at the build); the pair not accepted.
#[tokio::test]
async fn an_admin_revoke_names_the_author_as_his_message_arrived() {
    let rig = Rig::new().await;
    let conv = group();
    we_are_in_the_group_as(&rig, true, false).await;
    rig.store()
        .write("seed the pair and the ledger", |tx| {
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 1, 1, 'test')",
                kourier_db::rusqlite::params![AUTHOR_LID, AUTHOR_PN],
            )
            .map_err(crate::error::sql("learn"))?;
            // Another member's row with the same id: not his message.
            for sender in [
                "15550141119203@s.whatsapp.net".to_owned(),
                format!("{AUTHOR_PN}:7@s.whatsapp.net"),
            ] {
                tx.execute(
                    "INSERT INTO inbox(chat, sender, id, origin, received_at, catch_up, state) \
                     VALUES (?1, ?2, ?3, 'e2e', 1, 0, 'done')",
                    kourier_db::rusqlite::params![format!("{GROUP}@g.us"), sender, ORIGINAL],
                )
                .map_err(crate::error::sql("ledger"))?;
            }
            Ok(())
        })
        .await
        .expect("seed");
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let attempt = rig.spawn(admin_delete(&conv, LOCAL));
    rig.frames(1).await;
    let key = rig.session.sent()[0]
        .message
        .protocol_message
        .as_option()
        .and_then(|p| p.key.as_option().cloned())
        .expect("the key");
    assert_eq!(key.from_me, Some(false));
    assert_eq!(
        key.participant.as_deref(),
        Some(format!("{AUTHOR_PN}@s.whatsapp.net").as_str()),
        "the number his message arrived under, not the group's LID mode"
    );
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
    // No ledger row for this id: the current mode, as before.
    rig.session.script_group_mode(Ok(Namespace::Lid));
    let mut other = admin_delete(&conv, OTHER_LOCAL);
    if let OutgoingKind::Delete { target, .. } = &mut other.kind {
        target.local = "3EB0E1F2A3B4C5D6E7F8".into();
    }
    let attempt = rig.spawn(other);
    rig.frames(2).await;
    let key = rig.session.sent()[1]
        .message
        .protocol_message
        .as_option()
        .and_then(|p| p.key.as_option().cloned())
        .expect("the key");
    assert_eq!(
        key.participant.as_deref(),
        Some(format!("{AUTHOR_LID}@lid").as_str()),
        "no ledger row: the group's mode"
    );
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// W5b (G1, G2): the point of no return sits right before the frame. A stop that came first ends the attempt there
/// with `Failed(Internal)` in Signal's words, NO frame and nothing recorded; once the frame is written an abort is too
/// late and the message goes on to its ack. Mutants: the check removed; the point not closed (the stop only looked at).
#[tokio::test]
async fn a_stop_before_the_point_writes_no_frame_and_an_abort_after_it_is_too_late() {
    use kourier_backend_api::AbortAnswer;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let stopped = Stop::open();
    let parts = Parts {
        stop: Registered::alone(stopped.clone()),
        files: Files::none(),
        ..rig.parts()
    };
    assert_eq!(stopped.request(), AbortAnswer::Stopping);
    let outcome = spawn_with(parts, text(&conv, LOCAL)).await.expect("ends");
    let TransmitOutcome::Failed(e) = &outcome else {
        panic!("a stopped attempt fails: {outcome:?}");
    };
    assert_eq!(e.kind, ErrorKind::Internal, "the core keys on its own mark");
    assert_eq!(e.detail, "stopped by the user before anything was sent");
    assert!(rig.session.sent().is_empty(), "no frame after a stop");
    assert!(rig.looked(&conv, LOCAL).await.is_none(), "nothing recorded");

    let open = Stop::open();
    let parts = Parts {
        stop: Registered::alone(open.clone()),
        files: Files::none(),
        ..rig.parts()
    };
    let attempt = spawn_with(parts, text(&conv, OTHER_LOCAL));
    rig.frames(1).await;
    assert_eq!(
        open.request(),
        AbortAnswer::TooLate,
        "the frame is written: too late"
    );
    rig.ack(
        OTHER_LOCAL,
        Answer::Acked {
            server_t: Some(1_761_500_007),
        },
    );
    assert_eq!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent {
            server_at: Some(1_761_500_007_000),
            per_recipient: Vec::new(),
        },
        "the message went on"
    );
}

/// W5b B10 (brief §2.7): a caption edit of a file KOURIER sent carries no pointer (the core never stores an own one);
/// the attempt's one store read finds the TARGET's upload record and the edit writes that file with the new caption.
/// Without the record nothing is written (`Unsupported`, no frame). Mutant: the record not handed to the builder.
#[tokio::test]
async fn a_caption_edit_of_a_file_kourier_sent_is_written_from_its_upload_record() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let original = "3EB0C4F1E5D2B7A93018";
    rig.sent_before(&conv, original, 60).await;
    let caption_edit = |local: &str| OutgoingRequest {
        sent_at: NOW - 1_000,
        ..req(
            &conv,
            local,
            OutgoingKind::Edit {
                target: MsgTarget {
                    conv: Some(conv.clone()),
                    author: TargetAuthor::Own,
                    local: original.into(),
                    alternates: Vec::new(),
                },
                body: Body::Media {
                    attachments: vec![kourier_model::Attachment {
                        kind: kourier_model::AttachmentKind::Image,
                        mime: "image/png".into(),
                        size: 812_345,
                        ..kourier_model::Attachment::default()
                    }],
                    caption: Some(Text::plain("synthetic new caption")),
                },
            },
        )
    };
    // No record: no file to write.
    let outcome = rig
        .spawn(caption_edit("3EB0C4F1E5D2B7A93019"))
        .await
        .expect("ends");
    assert_eq!(failed_kind(&outcome), ErrorKind::Unsupported);
    assert!(rig.session.sent().is_empty(), "no frame without the file");

    let pointer = crate::media::MediaPointer {
        media_type: "image".into(),
        direct_path: "/v/t62.7118-24/synthetic-sent-52918374".into(),
        media_key: "5a".repeat(32),
        file_length: 61_234,
        media_key_timestamp: Some(1_761_400_000),
        mimetype: Some("image/jpeg".into()),
        width: Some(2048),
        height: Some(1536),
        shape: Some("image".into()),
        ..crate::media::MediaPointer::default()
    };
    let at = NOW - 60_000;
    crate::send::record::commit(&rig.store(), conv.clone(), original.into(), pointer, at)
        .await
        .expect("the record");
    let attempt = rig.spawn(caption_edit(LOCAL));
    rig.frames(1).await;
    let frame = &rig.session.sent()[0];
    let image = frame
        .message
        .protocol_message
        .as_option()
        .and_then(|p| p.edited_message.as_option())
        .and_then(|m| m.image_message.as_option())
        .expect("the picture written again");
    assert_eq!(
        image.direct_path.as_deref(),
        Some("/v/t62.7118-24/synthetic-sent-52918374")
    );
    assert_eq!(image.mimetype.as_deref(), Some("image/jpeg"));
    assert_eq!(image.caption.as_deref(), Some("synthetic new caption"));
    rig.ack(
        LOCAL,
        Answer::Acked {
            server_t: Some(NOW / 1000),
        },
    );
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

// ----- the file work (W5b, B11) -----

/// The directories of the file work and the events it reports to.
struct FileRig {
    _root: tempfile::TempDir,
    blobs: std::path::PathBuf,
    tmp: std::path::PathBuf,
    events: tokio::sync::mpsc::Receiver<kourier_backend_api::Envelope>,
    files: Option<Files>,
}

fn file_rig() -> FileRig {
    let root = tempfile::tempdir().expect("tempdir");
    let blobs = root.path().join("blobs");
    std::fs::create_dir_all(blobs.join("ab")).expect("blob dir");
    let tmp = crate::send::tmp::dir_beside(&root.path().join("backend.db"));
    let (events, rx) = tokio::sync::mpsc::channel(256);
    let files = Files {
        events,
        blob_dir: blobs.clone(),
        tmp_dir: tmp.clone(),
        lingering: crate::send::upload::Lingering::default(),
    };
    FileRig {
        _root: root,
        blobs,
        tmp,
        events: rx,
        files: Some(files),
    }
}

impl FileRig {
    /// A synthetic picture as the core's file (a kourier-blob in the blob directory).
    fn picture(&self) -> kourier_model::Attachment {
        let bytes =
            kourier_media::sample_image(320, 240, 52_830_174, kourier_media::SampleFormat::Jpeg)
                .expect("sample");
        self.file(kourier_model::AttachmentKind::Image, "image/jpeg", &bytes)
    }

    /// `bytes` as the core's file of `kind` (a kourier-blob in the blob directory).
    fn file(
        &self,
        kind: kourier_model::AttachmentKind,
        mime: &str,
        bytes: &[u8],
    ) -> kourier_model::Attachment {
        use std::io::Write as _;
        let key = [0x5D; 32];
        let rel = std::path::PathBuf::from("ab").join("core-file-39017452");
        let mut writer = kourier_blob::BlobWriter::create(
            &self.blobs.join(&rel),
            &kourier_blob::FileKey::from_bytes(key),
        )
        .expect("writer");
        writer.write_all(bytes).expect("write");
        writer.finish_volatile().expect("finish");
        kourier_model::Attachment {
            kind,
            mime: mime.into(),
            size: bytes.len() as u64,
            local: Some(kourier_model::BlobRef {
                path: rel,
                key: kourier_model::BlobKey::new(key),
            }),
            ..kourier_model::Attachment::default()
        }
    }

    /// The account's lingering POSTs this rig's attempt registers in (fix F5); `share` hands them to a second rig,
    /// as the actor shares one set between the attempts of an account.
    fn lingering(&self) -> crate::send::upload::Lingering {
        self.files
            .as_ref()
            .expect("the rig's files")
            .lingering
            .clone()
    }

    fn share(&mut self, lingering: crate::send::upload::Lingering) {
        self.files.as_mut().expect("the rig's files").lingering = lingering;
    }

    /// The parts of a file message's attempt (its stop open, as the actor gives it).
    fn parts(&mut self, rig: &Rig, stop: &Stop) -> Parts {
        Parts {
            stop: Registered::alone(stop.clone()),
            files: self.files.take().expect("one attempt per file rig"),
            ..rig.parts()
        }
    }

    /// The temporary files left (none once the attempt ended).
    fn temporary(&self) -> usize {
        std::fs::read_dir(&self.tmp).map_or(0, |dir| dir.count())
    }

    /// Every event reported so far.
    fn reported(&mut self) -> Vec<kourier_backend_api::BackendEvent> {
        let mut out = Vec::new();
        while let Ok(envelope) = self.events.try_recv() {
            out.extend(envelope.events);
        }
        out
    }
}

fn file_message(
    conv: &ConvKey,
    local: &str,
    attachments: Vec<kourier_model::Attachment>,
) -> OutgoingRequest {
    req(
        conv,
        local,
        OutgoingKind::Message {
            body: Body::Media {
                attachments,
                caption: Some(Text::plain("synthetic caption")),
            },
            reply_to: None,
            expiry: None,
        },
    )
}

async fn record_of(rig: &Rig, local: &str) -> Option<crate::send::record::Stored> {
    let local = local.to_owned();
    rig.store()
        .read(move |conn| crate::send::record::look(conn, &local))
        .await
        .expect("read")
}

fn sent_picture(frame: &crate::session::Outgoing) -> waproto::whatsapp::message::ImageMessage {
    frame
        .message
        .image_message
        .as_option()
        .expect("a picture message")
        .clone()
}

/// W5b B11 (G1, G4; brief §2.1): a message with a file is PREPARED and UPLOADED after every gate, its upload RECORDED
/// before the frame (the record is there while the frame waits for its ack), and the frame carries the uploaded file
/// with its caption. The reports start with `Preparing` and end with the file whole and `TransmitProgress{1.0, None}`
/// before the frame; no temporary file is left. Mutant: the record written under another key (not this message's).
#[tokio::test]
async fn a_file_is_uploaded_and_recorded_before_its_frame() {
    use kourier_backend_api::BackendEvent;
    use kourier_model::SendPhase;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let mut files = file_rig();
    let picture = files.picture();
    let stop = Stop::open();
    let attempt = spawn_with(
        files.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture]),
    );
    rig.frames(1).await;
    let calls = rig.session.calls();
    let upload_at = calls
        .iter()
        .position(|c| matches!(c, Call::Upload(crate::send::route::Shape::Image)))
        .expect("an upload of a picture");
    let send_at = calls
        .iter()
        .position(|c| matches!(c, Call::Send(_)))
        .expect("a frame");
    assert!(upload_at < send_at, "the upload before the frame");
    let stored = record_of(&rig, LOCAL)
        .await
        .expect("the upload is recorded before the frame's ack");
    let frame = &rig.session.sent()[0];
    assert_eq!(frame.id, LOCAL);
    let image = sent_picture(frame);
    let path = crate::session::fake::fake_direct_path(1);
    assert_eq!(image.direct_path.as_deref(), Some(path.as_str()));
    assert_eq!(stored.pointer.direct_path, path);
    assert_eq!(image.caption.as_deref(), Some("synthetic caption"));
    assert_eq!(image.mimetype.as_deref(), Some("image/jpeg"));
    assert_eq!(stored.at_ms, NOW);
    let events = files.reported();
    assert!(
        matches!(
            events.first(),
            Some(BackendEvent::TransmitProgress {
                phase: Some(SendPhase::Preparing),
                ..
            })
        ),
        "Preparing first"
    );
    let total = stored.pointer.file_length;
    let n = events.len();
    assert!(n >= 3);
    assert!(matches!(
        &events[n - 2],
        BackendEvent::AttachmentProgress { done, total: t, .. } if *done == total && *t == total
    ));
    assert!(matches!(
        &events[n - 1],
        BackendEvent::TransmitProgress {
            phase: None,
            progress,
            ..
        } if (*progress - 1.0).abs() < f32::EPSILON
    ));
    assert_eq!(files.temporary(), 0, "no temporary file left");
    rig.ack(
        LOCAL,
        Answer::Acked {
            server_t: Some(1_761_500_011),
        },
    );
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// W5b B11 (G4, brief §2.2 "DURABILITY"): a resend of a message whose upload is recorded sends the SAME file again with
/// no upload while the record is younger than 6 hours, reported whole at once; past 6 hours it uploads again. Mutants:
/// the record never reused; the reused record not reported.
#[tokio::test]
async fn a_resend_sends_its_recorded_file_again_within_six_hours_and_uploads_after() {
    use kourier_backend_api::BackendEvent;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let recorded = crate::media::MediaPointer {
        media_type: "image".into(),
        direct_path: "/v/t62.7118-24/synthetic-recorded-40617285".into(),
        media_key: "6b".repeat(32),
        file_sha256: Some("1c".repeat(32)),
        file_enc_sha256: Some("2d".repeat(32)),
        file_length: 48_213,
        media_key_timestamp: Some(1_761_400_000),
        mimetype: Some("image/jpeg".into()),
        width: Some(320),
        height: Some(240),
        shape: Some("image".into()),
        ..crate::media::MediaPointer::default()
    };
    // Within 6 hours: the record.
    crate::send::record::commit(
        &rig.store(),
        conv.clone(),
        LOCAL.into(),
        recorded.clone(),
        NOW - 3_600_000,
    )
    .await
    .expect("record");
    let mut files = file_rig();
    let picture = files.picture();
    let stop = Stop::open();
    let attempt = spawn_with(
        files.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture.clone()]),
    );
    rig.frames(1).await;
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        0,
        "no upload: the recorded file"
    );
    let image = sent_picture(&rig.session.sent()[0]);
    assert_eq!(
        image.direct_path.as_deref(),
        Some("/v/t62.7118-24/synthetic-recorded-40617285")
    );
    assert_eq!(image.file_length, Some(48_213));
    let events = files.reported();
    assert!(
        matches!(
            events.first(),
            Some(BackendEvent::AttachmentProgress {
                done: 48_213,
                total: 48_213,
                ..
            })
        ),
        "a reused record is reported whole at once: {}",
        events.len()
    );
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    attempt.await.expect("ends");

    // Past 6 hours: a new upload (the record is replaced by it).
    crate::send::record::commit(
        &rig.store(),
        conv.clone(),
        OTHER_LOCAL.into(),
        recorded,
        NOW - crate::send::record::REUSE_MS - 1,
    )
    .await
    .expect("record");
    let mut files = file_rig();
    let picture = files.picture();
    let attempt = spawn_with(
        files.parts(&rig, &stop),
        file_message(&conv, OTHER_LOCAL, vec![picture]),
    );
    rig.frames(2).await;
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        1,
        "an old record is uploaded again"
    );
    let image = sent_picture(&rig.session.sent()[1]);
    let path = crate::session::fake::fake_direct_path(1);
    assert_eq!(image.direct_path.as_deref(), Some(path.as_str()));
    assert_eq!(
        record_of(&rig, OTHER_LOCAL)
            .await
            .expect("record")
            .pointer
            .direct_path,
        path
    );
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    attempt.await.expect("ends");
}

/// W5b B11 (G2; brief §5 "an abort while uploading"): the person's stop while the upload runs is `Stopping` at once,
/// the upload is dropped where it stands, the attempt answers `Failed(Internal)` in Signal's words, no frame, no
/// record, no temporary file. Mutant: the upload not run under the stop (it would wait for the CDN for ever).
#[tokio::test]
async fn an_abort_while_uploading_stops_with_no_frame_no_record_and_no_file() {
    use crate::session::fake::UploadHold;
    use kourier_backend_api::AbortAnswer;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let _gate = rig.session.hold_uploads(UploadHold::AfterBytes(1024));
    let mut files = file_rig();
    let picture = files.picture();
    let stop = Stop::open();
    let attempt = spawn_with(
        files.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture]),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while rig
            .session
            .count(&Call::Upload(crate::send::route::Shape::Image))
            == 0
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the upload began");
    assert_eq!(stop.request(), AbortAnswer::Stopping);
    let outcome = tokio::time::timeout(Duration::from_secs(10), attempt)
        .await
        .expect("a stopped upload ends at once")
        .expect("ends");
    let TransmitOutcome::Failed(e) = &outcome else {
        panic!("a stopped attempt fails: {outcome:?}");
    };
    assert_eq!(e.kind, ErrorKind::Internal);
    assert_eq!(e.detail, "stopped by the user before anything was sent");
    assert!(rig.session.sent().is_empty(), "no frame");
    assert!(record_of(&rig, LOCAL).await.is_none(), "no record");
    tokio::time::timeout(Duration::from_secs(10), async {
        while files.temporary() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("no temporary file left");
}

/// W5b B11 (G10): a message with more than one file uploads NOTHING (the build refuses it after the gates, and the
/// file work runs only for exactly one file) and fails for good with no frame. Mutant: the file work for the first of
/// several.
#[tokio::test]
async fn a_message_with_two_files_uploads_nothing_and_is_refused() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let mut files = file_rig();
    let picture = files.picture();
    let stop = Stop::open();
    let outcome = spawn_with(
        files.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture.clone(), picture]),
    )
    .await
    .expect("ends");
    assert_eq!(failed_kind(&outcome), ErrorKind::Unsupported);
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        0
    );
    assert!(rig.session.sent().is_empty());
    assert!(record_of(&rig, LOCAL).await.is_none());
}

// ----- the W5b fix round (F1, F3, F4, F5) -----

/// The message row every `Registered::alone` attempt of these tests is for.
fn stopped_local() -> kourier_model::LocalMsgId {
    kourier_model::MessageId(0)
}

/// Waits until `check` holds (10 s of the wall clock at most).
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// Fix F1 (robustness HIGH): the person pressed X and was told `Stopping`; every way out of the part before the frame
/// must answer as a stop, never `Transient` (the core resends a transient answer, and the message they stopped would
/// go out). Here the upload is done and the X is accepted just as a lock is pushed: the hold's re-check (the third look
/// at the gate: the start, after the prepare, before the frame) closes it, and the answer is `Failed(Internal,
/// "stopped by the user before anything was sent")` with NO frame. Mutant: the guard removed (the answer is
/// `Transient{HOLD_RETRY}`).
#[tokio::test]
async fn a_stop_the_person_was_told_is_not_answered_transient_by_the_hold_after_the_upload() {
    use kourier_backend_api::AbortAnswer;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let stop = Stop::open();
    let looks = Arc::new(AtomicUsize::new(0));
    let told = Arc::new(Mutex::new(Vec::new()));
    let hold = {
        let (stop, looks, told) = (stop.clone(), looks.clone(), told.clone());
        Recheck::new(move || {
            if looks.fetch_add(1, Ordering::SeqCst) < 2 {
                return Gate::Open;
            }
            told.lock().expect("lock").push(stop.request());
            Gate::Held
        })
    };
    let mut files = file_rig();
    let picture = files.picture();
    let parts = Parts {
        hold,
        ..files.parts(&rig, &stop)
    };
    let outcome = spawn_with(parts, file_message(&conv, LOCAL, vec![picture]))
        .await
        .expect("ends");
    assert_eq!(looks.load(Ordering::SeqCst), 3, "the re-check was reached");
    assert_eq!(*told.lock().expect("lock"), vec![AbortAnswer::Stopping]);
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        1,
        "the file was up before the X"
    );
    let TransmitOutcome::Failed(e) = &outcome else {
        panic!("a stopped attempt fails: {outcome:?}");
    };
    assert_eq!(e.kind, ErrorKind::Internal, "Signal's kind, not transient");
    assert!(!e.is_retryable());
    assert_eq!(e.detail, "stopped by the user before anything was sent");
    assert!(rig.session.sent().is_empty(), "no frame after a stop");
}

/// Fix F3 (privacy LOW): the hold is asked again between the prepare and the upload. A lock another frame earned while
/// the file was prepared answers `Transient{HOLD_RETRY}` with NO upload call (no `w:m` IQ, no POST) and no temporary
/// file left. Mutant: the look removed (the file is uploaded into the lock).
#[tokio::test]
async fn a_lock_earned_during_the_prepare_keeps_the_upload_from_starting() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let looks = Arc::new(AtomicUsize::new(0));
    let hold = {
        let looks = looks.clone();
        Recheck::new(move || {
            if looks.fetch_add(1, Ordering::SeqCst) < 1 {
                Gate::Open
            } else {
                Gate::Held
            }
        })
    };
    let mut files = file_rig();
    let picture = files.picture();
    let stop = Stop::open();
    let parts = Parts {
        hold,
        ..files.parts(&rig, &stop)
    };
    let outcome = spawn_with(parts, file_message(&conv, LOCAL, vec![picture]))
        .await
        .expect("ends");
    assert_eq!(
        failed_kind(&outcome),
        ErrorKind::Transient {
            retry_after: Some(HOLD_RETRY)
        }
    );
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        0,
        "no upload into a lock"
    );
    assert!(rig.session.sent().is_empty());
    assert!(record_of(&rig, LOCAL).await.is_none());
    assert_eq!(
        files.temporary(),
        0,
        "the prepared file went with the return"
    );
}

/// Fix F4 (robustness LOW): the build's refusals that the pointer does not decide come BEFORE the file work. A file
/// message quoting a message of ANOTHER chat is refused (`Unsupported`) with no `Preparing`, no prepare, no upload,
/// no temporary file, no record and no frame. Mutant: the pre-check removed (the file is uploaded, then refused).
#[tokio::test]
async fn a_file_message_the_build_would_refuse_uploads_nothing() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let mut files = file_rig();
    let picture = files.picture();
    let mut request = file_message(&conv, LOCAL, vec![picture]);
    let OutgoingKind::Message { reply_to, .. } = &mut request.kind else {
        unreachable!("a message");
    };
    *reply_to = Some(ReplyTo {
        target: MsgTarget {
            conv: Some(dm(STRANGER_PN)),
            author: TargetAuthor::Own,
            local: ORIGINAL.into(),
            alternates: Vec::new(),
        },
        author: None,
        snapshot: None,
    });
    let stop = Stop::open();
    let outcome = spawn_with(files.parts(&rig, &stop), request)
        .await
        .expect("ends");
    assert_eq!(failed_kind(&outcome), ErrorKind::Unsupported);
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        0,
        "nothing uploaded for a message that is refused"
    );
    assert!(files.reported().is_empty(), "not even Preparing");
    assert_eq!(files.temporary(), 0);
    assert!(rig.session.sent().is_empty());
    assert!(record_of(&rig, LOCAL).await.is_none());
}

/// Fix F4: `captioned` is the fact of the RENDERED caption in the attempt as in the build. A caption that is not blank
/// but renders to nothing (a mention placeholder naming nobody) is NO caption: an Opus Ogg then goes as `audioMessage`
/// (an audio has none), not as a document. Mutant: `captioned` taken from the caption's body, not its rendering.
#[tokio::test]
async fn a_caption_that_renders_empty_leaves_an_audio_an_audio() {
    use kourier_model::text::MENTION_PLACEHOLDER;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let mut ogg = b"OggS\0\x02".to_vec();
    ogg.extend_from_slice(&[0u8; 20]);
    ogg.push(1);
    ogg.push(19);
    ogg.extend_from_slice(b"OpusHead\x01\x02\0\0\x80\xbb\0\0\0\0\0");
    ogg.extend_from_slice(&[0x45; 3000]);
    let mut files = file_rig();
    let audio = files.file(kourier_model::AttachmentKind::Audio, "audio/ogg", &ogg);
    let mut request = file_message(&conv, LOCAL, vec![audio]);
    let OutgoingKind::Message { body, .. } = &mut request.kind else {
        unreachable!("a message");
    };
    let Body::Media { caption, .. } = body else {
        unreachable!("a file");
    };
    let blank = Text::plain(MENTION_PLACEHOLDER.to_string());
    assert!(!blank.body.trim().is_empty(), "not blank by its body");
    *caption = Some(blank);
    let stop = Stop::open();
    let attempt = spawn_with(files.parts(&rig, &stop), request);
    rig.frames(1).await;
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Audio)),
        1,
        "uploaded as an audio"
    );
    let frame = &rig.session.sent()[0];
    assert!(
        frame.message.audio_message.as_option().is_some(),
        "an audio message"
    );
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// Fix F5 (robustness LOW): a POST blocked in a socket write outlives its attempt (upstream cannot cancel it), holding a
/// blocking thread and its sealed temporary file. While an earlier POST of the SAME local still runs, a new attempt of
/// that local answers `Transient{STALL_RETRY}` BEFORE any file work: no `Preparing`, no prepare (no temporary file),
/// no upload call. Once the thread ends the resend uploads and goes out. Another local is not limited (a named known
/// limit). Mutants: the guard never set (the second attempt uploads).
#[tokio::test]
async fn a_resend_waits_while_the_earlier_post_of_its_local_still_runs() {
    use crate::send::route::Shape;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let end = rig.session.linger_next_upload();
    let mut first = file_rig();
    let lingering = first.lingering();
    let picture = first.picture();
    let stop = Stop::open();
    let stalled = spawn_with(
        first.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture]),
    )
    .await
    .expect("ends");
    assert_eq!(
        failed_kind(&stalled),
        ErrorKind::Transient {
            retry_after: Some(crate::send::upload::STALL_RETRY)
        },
        "an upload that did not complete waits its floor"
    );
    assert_eq!(rig.session.count(&Call::Upload(Shape::Image)), 1);
    assert!(
        lingering.holds(stopped_local()),
        "the POST's thread still holds the file"
    );

    // The resend: nothing of it touches the file, the CDN or the disk.
    let mut second = file_rig();
    second.share(lingering.clone());
    let picture = second.picture();
    let stop = Stop::open();
    let waits = spawn_with(
        second.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture]),
    )
    .await
    .expect("ends");
    assert_eq!(
        failed_kind(&waits),
        ErrorKind::Transient {
            retry_after: Some(crate::send::upload::STALL_RETRY)
        }
    );
    assert_eq!(
        rig.session.count(&Call::Upload(Shape::Image)),
        1,
        "no second upload while the first thread runs"
    );
    assert!(second.reported().is_empty(), "not even Preparing");
    assert_eq!(second.temporary(), 0, "no second temporary file");
    assert!(rig.session.sent().is_empty());

    // The thread ends: the resend uploads and goes out.
    end.send(()).expect("the parked thread listens");
    until("the thread ends", || !lingering.holds(stopped_local())).await;
    let mut third = file_rig();
    third.share(lingering.clone());
    let picture = third.picture();
    let stop = Stop::open();
    let attempt = spawn_with(
        third.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture]),
    );
    rig.frames(1).await;
    assert_eq!(rig.session.count(&Call::Upload(Shape::Image)), 2);
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    assert!(matches!(
        attempt.await.expect("ends"),
        TransmitOutcome::Sent { .. }
    ));
}

/// A hold that is open for its first two looks (the start and after the prepare) and, at the third (the re-check
/// right before the frame), runs `third` and answers `gate`.
fn hold_at_the_recheck(
    gate: Gate,
    third: impl Fn() + Send + Sync + 'static,
) -> (Recheck, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let looks = Arc::new(AtomicUsize::new(0));
    let hold = {
        let looks = looks.clone();
        Recheck::new(move || {
            if looks.fetch_add(1, Ordering::SeqCst) < 2 {
                return Gate::Open;
            }
            third();
            gate
        })
    };
    (hold, looks)
}

/// Fix F13 (privacy MEDIUM), the stop that wins at the hold's re-check: the upload is done and its record committed,
/// then the person's X is accepted just as a lock closes the gate. The message ends for good (`stopped`), so its
/// upload record (media key, paths, preview) is deleted before the answer. Mutant: the delete on this way out removed
/// (the row stays for the 30 days of the sweep).
#[tokio::test]
async fn a_file_stopped_at_the_holds_recheck_leaves_no_upload_record() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let stop = Stop::open();
    let (hold, _) = hold_at_the_recheck(Gate::Held, {
        let stop = stop.clone();
        move || {
            let _ = stop.request();
        }
    });
    let mut files = file_rig();
    let picture = files.picture();
    let parts = Parts {
        hold,
        ..files.parts(&rig, &stop)
    };
    let outcome = spawn_with(parts, file_message(&conv, LOCAL, vec![picture]))
        .await
        .expect("ends");
    assert_eq!(failed_kind(&outcome), ErrorKind::Internal, "a stop");
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        1,
        "the upload was done, so its record was written"
    );
    assert!(
        record_of(&rig, LOCAL).await.is_none(),
        "the record of a stopped message was kept"
    );
    assert!(rig.session.sent().is_empty());
}

/// Fix F13, the stop that wins at the point of no return: the gate is open at the re-check, the X is accepted right
/// after it, `stop.close()` fails. The record is deleted (the attempt ends stopped). Mutant: the delete on this way
/// out removed.
#[tokio::test]
async fn a_file_stopped_at_the_point_of_no_return_leaves_no_upload_record() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let stop = Stop::open();
    let (hold, looks) = hold_at_the_recheck(Gate::Open, {
        let stop = stop.clone();
        move || {
            let _ = stop.request();
        }
    });
    let mut files = file_rig();
    let picture = files.picture();
    let parts = Parts {
        hold,
        ..files.parts(&rig, &stop)
    };
    let outcome = spawn_with(parts, file_message(&conv, LOCAL, vec![picture]))
        .await
        .expect("ends");
    assert_eq!(looks.load(std::sync::atomic::Ordering::SeqCst), 3);
    let TransmitOutcome::Failed(e) = &outcome else {
        panic!("a stopped attempt fails: {outcome:?}");
    };
    assert_eq!(e.detail, "stopped by the user before anything was sent");
    assert!(rig.session.sent().is_empty(), "no frame after a stop");
    assert!(
        record_of(&rig, LOCAL).await.is_none(),
        "the record of a stopped message was kept"
    );
}

/// Fix F13, what must NOT go: a transient end keeps the record (the resend within 6 hours sends the same upload again
/// instead of uploading a second time). Here the lock closes at the re-check, no stop. Mutant: the delete on every
/// failure (not only the ones for good).
#[tokio::test]
async fn a_transient_end_keeps_the_upload_record_for_the_resend() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let stop = Stop::open();
    let (hold, _) = hold_at_the_recheck(Gate::Held, || {});
    let mut files = file_rig();
    let picture = files.picture();
    let parts = Parts {
        hold,
        ..files.parts(&rig, &stop)
    };
    let outcome = spawn_with(parts, file_message(&conv, LOCAL, vec![picture]))
        .await
        .expect("ends");
    assert_eq!(
        failed_kind(&outcome),
        ErrorKind::Transient {
            retry_after: Some(HOLD_RETRY)
        }
    );
    assert!(
        record_of(&rig, LOCAL).await.is_some(),
        "a transient end lost the record its resend reuses"
    );
}

/// Fix F16 (robustness MEDIUM, Q-B16), the quit during an upload: the attempt ends at once, nothing is written, and its
/// answer is `Transient` with no wait in the quit's words, NOT the person's `Internal` stop (the core shows that as
/// failed for good and never sends the message again). No frame, no record (the upload was not done), no temporary
/// file. Mutants: the quit not noticed by the upload's wait (`is_ended` -> `is_requested`); the quit answered
/// `stopped()`.
#[tokio::test]
async fn a_quit_while_uploading_ends_the_attempt_transient_with_no_frame_no_record_and_no_file() {
    use crate::session::fake::UploadHold;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let _gate = rig.session.hold_uploads(UploadHold::AfterBytes(1024));
    let mut files = file_rig();
    let picture = files.picture();
    let stop = Stop::open();
    let attempt = spawn_with(
        files.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture]),
    );
    until("the upload began", || {
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image))
            == 1
    })
    .await;
    assert!(stop.quit(), "the attempt is before its point of no return");
    let outcome = tokio::time::timeout(Duration::from_secs(10), attempt)
        .await
        .expect("a quit ends an upload at once")
        .expect("ends");
    let TransmitOutcome::Failed(e) = &outcome else {
        panic!("a quit attempt fails: {outcome:?}");
    };
    assert_eq!(e.kind, ErrorKind::Transient { retry_after: None });
    assert!(e.is_retryable(), "the core sends the message again");
    assert_eq!(e.detail, crate::send::stop::quitting().detail);
    assert!(rig.session.sent().is_empty(), "no frame");
    assert!(record_of(&rig, LOCAL).await.is_none(), "no record");
    until("no temporary file left", || files.temporary() == 0).await;
}

/// Fix F16, the quit that wins at the point of no return: the gate is open at the re-check, the quit takes the attempt
/// right after it, `stop.close()` fails. The answer is `Transient` (the quit's words), no frame, and the upload record
/// STAYS (the resend within 6 hours sends the same upload again; a stop would delete it). Mutants: `close()` that lets
/// a quitting attempt pass (a frame goes out after the quit); the answer `stopped()` for every failed close (it
/// deletes the record and fails the message for good).
#[tokio::test]
async fn a_quit_at_the_point_of_no_return_writes_no_frame_and_keeps_the_upload_record() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let stop = Stop::open();
    let (hold, looks) = hold_at_the_recheck(Gate::Open, {
        let stop = stop.clone();
        move || {
            assert!(stop.quit());
        }
    });
    let mut files = file_rig();
    let picture = files.picture();
    let parts = Parts {
        hold,
        ..files.parts(&rig, &stop)
    };
    let outcome = spawn_with(parts, file_message(&conv, LOCAL, vec![picture]))
        .await
        .expect("ends");
    assert_eq!(looks.load(std::sync::atomic::Ordering::SeqCst), 3);
    let TransmitOutcome::Failed(e) = &outcome else {
        panic!("a quit attempt fails: {outcome:?}");
    };
    assert_eq!(e.kind, ErrorKind::Transient { retry_after: None });
    assert_eq!(e.detail, crate::send::stop::quitting().detail);
    assert!(rig.session.sent().is_empty(), "no frame after the quit");
    assert!(
        record_of(&rig, LOCAL).await.is_some(),
        "the quit lost the record its resend reuses"
    );
}

/// Fix F13, "the build fails for good" and what the delete may touch: [`forget_a_dead_record`] deletes the row of a new
/// message with a file when the answer is a failure for good, keeps it for a transient one, and never touches a
/// row for a request that is not a new message with a file (a caption edit reads its TARGET's row; no write at all). Mutants: the
/// delete removed; the new-file-message check removed (a failed text or edit writes a delete).
#[tokio::test]
async fn only_a_new_file_message_that_failed_for_good_deletes_its_upload_record() {
    use super::forget_a_dead_record;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let planted = |local: &'static str| {
        let store = rig.store();
        let conv = conv.clone();
        async move {
            store
                .write("test: plant", move |tx| {
                    crate::send::record::put(
                        tx,
                        &conv,
                        local,
                        &crate::media::MediaPointer {
                            media_type: "image".into(),
                            direct_path: "/v/t62.7118-24/synthetic-path-61830495".into(),
                            media_key: format!("{:0>64}", "61830495"),
                            file_length: 1,
                            ..crate::media::MediaPointer::default()
                        },
                        NOW,
                    )
                })
                .await
                .expect("plant");
        }
    };
    let file = file_message(&conv, LOCAL, vec![kourier_model::Attachment::default()]);
    let parts = rig.parts();
    let gone = TransmitOutcome::Failed(kourier_model::BackendError::unsupported("synthetic"));
    let later = TransmitOutcome::Failed(kourier_model::BackendError::transient("synthetic"));

    planted(LOCAL).await;
    forget_a_dead_record(&parts, &file, LOCAL, "Message", &later).await;
    assert!(record_of(&rig, LOCAL).await.is_some(), "a transient end");
    forget_a_dead_record(&parts, &file, LOCAL, "Message", &super::sent(None)).await;
    assert!(record_of(&rig, LOCAL).await.is_some(), "a Sent answer");
    forget_a_dead_record(&parts, &file, LOCAL, "Message", &gone).await;
    assert!(record_of(&rig, LOCAL).await.is_none(), "a failure for good");

    // Any other request writes nothing for its own local: a text, and a caption edit (whose record key is its TARGET's
    // row, which a failed edit must leave). A row planted under their own local shows that no delete ran.
    planted(OTHER_LOCAL).await;
    let edit = req(
        &conv,
        OTHER_LOCAL,
        OutgoingKind::Edit {
            target: MsgTarget {
                conv: Some(conv.clone()),
                author: TargetAuthor::Own,
                local: ORIGINAL.into(),
                alternates: Vec::new(),
            },
            body: Body::Media {
                attachments: Vec::new(),
                caption: Some(Text::plain("synthetic caption")),
            },
        },
    );
    forget_a_dead_record(&parts, &edit, OTHER_LOCAL, "Edit", &gone).await;
    forget_a_dead_record(
        &parts,
        &text(&conv, OTHER_LOCAL),
        OTHER_LOCAL,
        "Message",
        &gone,
    )
    .await;
    assert!(
        record_of(&rig, OTHER_LOCAL).await.is_some(),
        "a request that is no new file message deleted a record"
    );
}

/// Fix F17 (robustness LOW): ONE still picture is decoded at a time in the process. Two messages with pictures prepared
/// at once (two attempts, two locals) never overlap in the decode (the seam counts the pictures being decoded and
/// lingers inside, so an unkept pair would overlap); both are sent. Mutant: no permit taken.
#[tokio::test]
async fn two_pictures_prepared_at_once_are_never_decoded_together() {
    use std::sync::atomic::Ordering;

    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    crate::send::upload::MOST_DECODING.store(0, Ordering::SeqCst);
    let (mut first, mut second) = (file_rig(), file_rig());
    let (one, two) = (first.picture(), second.picture());
    let (stop_one, stop_two) = (Stop::open(), Stop::open());
    let a = spawn_with(
        first.parts(&rig, &stop_one),
        file_message(&conv, LOCAL, vec![one]),
    );
    let b = spawn_with(
        second.parts(&rig, &stop_two),
        file_message(&conv, OTHER_LOCAL, vec![two]),
    );
    rig.frames(2).await;
    rig.ack(LOCAL, Answer::Acked { server_t: None });
    rig.ack(OTHER_LOCAL, Answer::Acked { server_t: None });
    for attempt in [a, b] {
        assert!(matches!(
            attempt.await.expect("ends"),
            TransmitOutcome::Sent { .. }
        ));
    }
    assert_eq!(
        crate::send::upload::MOST_DECODING.load(Ordering::SeqCst),
        1,
        "two pictures were decoded at the same time"
    );
    assert_eq!(
        crate::send::upload::DECODING.load(Ordering::SeqCst),
        0,
        "a picture is still being decoded"
    );
}

/// Fix F17, under the stop: a picture that waits for the turn (another picture holds it) is stopped by the X at once:
/// `stopped`, no upload, no temporary file, no frame. Mutant: the wait for the turn not under the stop (the attempt
/// hangs until the turn is free).
#[tokio::test]
async fn an_x_pressed_while_a_picture_waits_for_its_turn_stops_it() {
    let rig = Rig::new().await;
    let conv = dm(PEER_PN);
    let holder = crate::send::upload::PICTURES
        .acquire()
        .await
        .expect("the picture turn");
    let mut files = file_rig();
    let picture = files.picture();
    let stop = Stop::open();
    let attempt = spawn_with(
        files.parts(&rig, &stop),
        file_message(&conv, LOCAL, vec![picture]),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!attempt.is_finished(), "the picture waits for the turn");
    assert_eq!(stop.request(), kourier_backend_api::AbortAnswer::Stopping);
    let outcome = tokio::time::timeout(Duration::from_secs(5), attempt)
        .await
        .expect("the X ends the wait for the turn")
        .expect("ends");
    drop(holder);
    let TransmitOutcome::Failed(e) = &outcome else {
        panic!("a stopped attempt fails: {outcome:?}");
    };
    assert_eq!(e.detail, "stopped by the user before anything was sent");
    assert_eq!(
        rig.session
            .count(&Call::Upload(crate::send::route::Shape::Image)),
        0
    );
    assert_eq!(files.temporary(), 0);
    assert!(rig.session.sent().is_empty());
}
