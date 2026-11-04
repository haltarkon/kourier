//! TH6a S8: THE KILL LOOPS OF THE SEND CORE through a REAL actor (brief §2.4 "THE WINDOWS", §2.12; decisions R1, R7, L7),
//! on the parent-resident fake (`support/piped.rs`, the form of `connection/kill.rs`). The proof of the step's sentence:
//!
//! After a kill at ANY window, the next attempt of the same `local` reuses the message id and `created_at`, draws no nonce,
//! seals nothing, and every frame it writes equals byte for byte a frame an earlier attempt wrote or would have written, only
//! where that frame's outcome is not durable; a receiver with a durable ack gets nothing; after `done` no frame goes at all;
//! the peer opens exactly one message per receiver and the phone maps one `OutgoingMessage`; the reflect precedes the first box.
//!
//! THE FORM: a fresh world per case, a 1:1 and a group of three receivers. A child under `unshare -rn` connects, sends a
//! `Transmit` through the parts' test door and dies at its armed failpoint. After EACH kill the PARENT reads the file and the
//! fake against the case's literal row ([`Kill`]): the row's state, each receiver's state, which sealed columns stand, both
//! nonce stores by their count, how many reflects and boxes the fake has. Then the next child asks for the same `local`, under
//! ANOTHER `sent_at` once a row is durable, so a request's time cannot pass for the row's. It reads two answers: its attempt and
//! the ledger's. At the end the parent compares the frames with each other before anything opens them, then with the bytes the
//! file held, lets the fake's peers and its phone read them, and compares the file with a no-kill reference run, row for row in
//! states and counts (nonces and padding differ per run: the seeded RNG does not reach libthreema's `thread_rng`).
//!
//! WHAT A LOOP CANNOT DRAW FROM TIMING: how many ZONE ACKS three acks make is the socket's passes. So a window that names a
//! partial pass HOLDS the acks of the others (`CspKnobs::withhold_message_ack_of`, before the spawn), which makes each zone hold
//! exactly one receiver and each state a literal; a group reaches W9 by such a walk of kills. A kill of an earlier child loses
//! the acks held for its socket.
//!
//! THE CLOCK: the fake's virtual clock moves [`STEP`] between two children (never while one runs), and a child's clock starts
//! there, so `at(n)` is what the n-th child's zones and reflect-acks say. `server_at` tells whose update it was.
//!
//! R1: a row at `acked` needs nobody. W9 to W11's last child waits until the row is `done` in the file BEFORE it sends its
//! `Transmit`, which is then answered from the ledger. A child's core never acks, but neither path needs it to: no `core_outbox`
//! row is written here, so `max_seq` is 0, `Online` comes with Dry, and a child waits for "past the login"; a `Transmit` needs the
//! login-ack only. The JOB loop rests on that same fact: a job runs from `Online`.
//!
//! The order of a loop's cases is not the table's where a mutant named after a window (L7) must die there and not at another that
//! also feels it: W5 and W12 first in theirs, W8 before the group's W6, whose acks may share a zone or not.
//! Nothing is printed but counts, states and window names; a child's answer names a receiver by its place in the list.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{Command, OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::rusqlite::types::ValueRef;
use kourier_db::rusqlite::{Connection, params};
use kourier_model::{Body, ConnState, ConvKey, MessageId, RecipientState, UserId};
use kourier_threema::failpoints as fp;
use kourier_threema::store::{
    self, NewBody, NewJob, ReceiverState, SendOrigin, SendReceiver, SendState, SendTask, To,
};
use libthreema::protobuf::common::GroupIdentity;
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::envelope::Content;
use libthreema::protobuf::d2d::outgoing_message_update::{Sent as SentMark, Update, update};
use prost::Message as _;

use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, ty};
use crate::support::mock::envelope::{ContactDelta, GroupDelta};
use crate::support::mock::peer::Discard;
use crate::support::mock::phone::open_envelope;
use crate::support::piped::{self, Ended, ME, Mode, Parent, Running, WALL};
use crate::support::{db_path, key};

/// The receivers, synthetic, in the order the rows sort them.
const ANN: &str = "S8KANN01";
const BOB: &str = "S8KBOB02";
const CRE: &str = "S8KCRE03";
const GROUP: u64 = 0x42;
/// A group's key, written out from the spec's form: the creator and the id's little-endian bytes in hex.
const GROUP_DIGITS: &str = "4200000000000000";

/// The message's id as the core names it, and the `MessageId.0` of its wire bytes 01..08.
const LOCAL: &str = "0102030405060708";
const ID: u64 = 0x0807_0605_0403_0201;
/// The first request's time, in ms and in seconds, earlier than any clock of the world.
const T0: i64 = 1_759_999_999_123;
const T0_S: u32 = 1_759_999_999;
/// A retry's `sent_at`, which no durable row may take.
const T_RETRY: i64 = T0 + 77_000;
/// A job's `created_at`: the clock of the zone that wrote it.
const JOB_S: u32 = 1_760_000_000;
const TEXT: &str = "synthetic kill text";
/// The flags of a text: send a push.
const PUSH: u8 = 0x01;

/// How far the fake's clock moves between two children of a case.
const STEP: Duration = Duration::from_secs(10);
/// The clock of a case's child `n`.
const fn at(n: u64) -> u64 {
    START_MS + n * 10_000
}

const SHAPE_ENV: &str = "THREEMA_S8_SHAPE";
const SENT_AT_ENV: &str = "THREEMA_S8_SENT_AT";
const ITSELF_ENV: &str = "THREEMA_S8_BY_ITSELF";
/// What starts a child's line with an answer.
const ANSWER: &str = "s8-answer ";
const ATTEMPT_CHILD: &str = "attempt_child";
const JOB_CHILD: &str = "job_child";

/// What a case sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Shape {
    /// A `Transmit` to a contact.
    Direct,
    /// A `Transmit` to a group: two members and the creator.
    Group,
    /// A job to a contact, in the file before the first child (W2 is its seed).
    Job,
}

impl Shape {
    const fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
            Self::Job => "job",
        }
    }

    const fn receivers(self) -> &'static [&'static str] {
        match self {
            Self::Direct | Self::Job => &[ANN],
            Self::Group => &[ANN, BOB, CRE],
        }
    }

    const fn ty(self) -> u8 {
        match self {
            Self::Direct | Self::Job => ty::TEXT,
            Self::Group => ty::GROUP_TEXT,
        }
    }

    /// The container's body, by the fake's own builders.
    fn body(self) -> Vec<u8> {
        match self {
            Self::Direct | Self::Job => body::text(TEXT),
            Self::Group => body::group_member_container(CRE, GROUP, &body::text(TEXT)),
        }
    }

    fn conversation(self) -> d2d::ConversationId {
        let id = match self {
            Self::Direct | Self::Job => d2d::conversation_id::Id::Contact(ANN.to_owned()),
            Self::Group => d2d::conversation_id::Id::Group(GroupIdentity {
                group_id: GROUP,
                creator_identity: CRE.to_owned(),
            }),
        };
        d2d::ConversationId { id: Some(id) }
    }

    /// A `Transmit` of the plain text, as the core's dispatcher makes it after a dropped reply: no `only`.
    fn request(self, sent_at: i64) -> OutgoingRequest {
        let conv = match self {
            Self::Direct | Self::Job => {
                ConvKey::Direct(UserId::from_parts("threema", ANN).expect("a user"))
            }
            Self::Group => ConvKey::Group(format!("threema:{CRE}.{GROUP_DIGITS}").into()),
        };
        OutgoingRequest {
            conv,
            local: Arc::from(LOCAL),
            sent_at,
            kind: OutgoingKind::Message {
                body: Body::text(TEXT),
                reply_to: None,
                expiry: None,
            },
            only: None,
            is_recipient_update: false,
        }
    }

    /// What a child's line says of `Sent { server_at: at_ms, every receiver }`.
    fn sent(self, at_ms: u64) -> String {
        let whom = match self {
            Self::Direct | Self::Job => "0",
            Self::Group => "0,1,2",
        };
        format!("Sent at Some({at_ms}) to {whom}")
    }
}

/// Which of a row's two reflects a frame is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reflect {
    Message,
    Update,
}
use Reflect::{Message as M, Update as U};

/// The file's literal rows: the row's state after a kill (`None`: the file holds no row).
const NO_ROW: Option<SendState> = None;
const PREPARED: Option<SendState> = Some(SendState::Prepared);
const REFLECTED: Option<SendState> = Some(SendState::Reflected);
const ACKED: Option<SendState> = Some(SendState::Acked);
const DONE: Option<SendState> = Some(SendState::Done);
const NOBODY: &[&str] = &[];

/// One kill and the literal state it leaves in the file and at the fake.
#[derive(Clone, Copy)]
struct Kill {
    point: &'static str,
    /// The point's k-th hit in that child.
    k: u32,
    /// The chat server holds the message-acks of these while this child runs.
    holds: &'static [&'static str],
    state: Option<SendState>,
    /// The receivers with a durable ack; every other receiver is `pending` and holds its box.
    acked: &'static [&'static str],
    /// `csp_nonce` and `d2d_nonce`, by count.
    nonces: (i64, i64),
    /// `Mediator::reflects` and `csp_sent`, by count.
    frames: (usize, usize),
}

const fn kill(
    point: &'static str,
    k: u32,
    state: Option<SendState>,
    acked: &'static [&'static str],
    nonces: (i64, i64),
    frames: (usize, usize),
) -> Kill {
    Kill {
        point,
        k,
        holds: NOBODY,
        state,
        acked,
        nonces,
        frames,
    }
}

impl Kill {
    const fn holding(mut self, holds: &'static [&'static str]) -> Self {
        self.holds = holds;
        self
    }
}

/// One world: its kills in order, a last child with nothing armed, and the literal end.
struct Case {
    what: &'static str,
    shape: Shape,
    kills: &'static [Kill],
    /// The last child waits until the row is `done` before it asks (R1).
    by_itself: bool,
    /// Every reflect the fake got, in order.
    reflects: &'static [Reflect],
    /// How many boxes the fake kept per receiver: one opened, the others discarded by their nonce.
    boxes: &'static [(&'static str, usize)],
    /// Which child's clock the row's `queued_at` is.
    prepared_by: u64,
    /// Which child's clock the update's reflect-ack and ZONE DONE are.
    sent_by: u64,
}

const fn case(
    what: &'static str,
    shape: Shape,
    kills: &'static [Kill],
    reflects: &'static [Reflect],
    boxes: &'static [(&'static str, usize)],
    prepared_by: u64,
    sent_by: u64,
) -> Case {
    Case {
        what,
        shape,
        kills,
        by_itself: false,
        reflects,
        boxes,
        prepared_by,
        sent_by,
    }
}

impl Case {
    const fn by_itself(mut self) -> Self {
        self.by_itself = true;
        self
    }
}

/// The world of `shape`: the stored contacts with their real keys, the group, and a job when it is one. Every world of these
/// loops is one seed; what differs between two runs is only what libthreema draws.
fn world(shape: Shape) -> Parent {
    let p = piped::parent(7_801);
    let db = p.open();
    db.zone(|tx| {
        for id in shape.receivers() {
            let peer = p.world.add_identity(id);
            let sync = ContactDelta::create(id, peer.ck.public, START_MS).build();
            tx.execute(
                "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
                params![id, sync.encode_to_vec()],
            )?;
        }
        if shape == Shape::Group {
            let sync =
                GroupDelta::create(CRE, GROUP, "synthetic group", &[ANN, BOB], START_MS).build();
            tx.execute(
                "INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES (?1, ?2, ?3, 0)",
                params![
                    CRE,
                    i64::from_ne_bytes(GROUP.to_ne_bytes()),
                    sync.encode_to_vec()
                ],
            )?;
        }
        Ok::<(), kourier_db::Error>(())
    })
    .expect("seed the receivers");
    if shape == Shape::Job {
        let job = NewJob {
            kind: "test-job",
            to: To::Contact(ANN.to_owned()),
            body: NewBody::Text(TEXT.to_owned()),
            only: None,
        };
        let now = i64::try_from(START_MS).expect("ms fit i64");
        db.zone(|tx| store::enqueue_job(tx, &job, now))
            .expect("seed the job");
    }
    drop(db);
    p
}

/// The chat server holds the acks of exactly these from here on. Acks held for a socket that died are lost.
fn hold(p: &Parent, shape: Shape, holds: &[&str]) {
    let m = p.world.mediator();
    for id in shape.receivers() {
        m.release_message_acks(id);
    }
    m.knobs(|k| {
        for id in holds {
            k.csp.withhold_message_ack_of(id);
        }
    });
}

/// The child of a case: armed or the last one.
fn spawn(p: &Parent, case: &Case, armed: Option<&str>, sent_at: i64) -> Option<Running> {
    if case.shape == Shape::Job {
        return p.spawn(module_path!(), JOB_CHILD, Mode::Resume, armed);
    }
    let sent_at = sent_at.to_string();
    let by_itself = if armed.is_none() && case.by_itself {
        "1"
    } else {
        "0"
    };
    p.spawn_with(
        module_path!(),
        ATTEMPT_CHILD,
        Mode::Resume,
        armed,
        &[
            (SHAPE_ENV, case.shape.name()),
            (SENT_AT_ENV, sent_at.as_str()),
            (ITSELF_ENV, by_itself),
        ],
    )
}

/// What the file held of the sealed bytes at the kills: what the frames at the end must equal.
#[derive(Default)]
struct Stored {
    message: Option<Vec<u8>>,
    update: Option<Vec<u8>>,
    boxes: BTreeMap<String, Vec<u8>>,
    /// The job's id, which the door drew.
    id: Option<u64>,
}

impl Stored {
    /// A stored form may disappear (the zones NULL it) but never change: nothing is sealed twice.
    fn keep(slot: &mut Option<Vec<u8>>, now: Option<&Vec<u8>>, what: &str) {
        let Some(now) = now else {
            return;
        };
        assert!(
            slot.as_ref().is_none_or(|before| before == now),
            "{what}: the stored bytes changed: something was sealed again"
        );
        *slot = Some(now.clone());
    }
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("a count")
}

/// THE WINDOW'S LITERAL ROW, after each kill: the file and the fake say what §2.4 says, and each frame so far is what the file
/// holds, where it still holds it.
fn check_kill(p: &Parent, case: &Case, kill: &Kill, stored: &mut Stored, what: &str) {
    let db = p.open();
    let tasks = db.read(store::send_tasks).expect("the tasks");
    assert!(tasks.len() <= 1, "{what}: a second row");
    let task = tasks.first();
    assert_eq!(task.map(|t| t.state), kill.state, "{what}: the row's state");
    let receivers: Vec<SendReceiver> = task.map_or_else(Vec::new, |t| {
        db.read(|c: &Connection| store::send_receivers(c, t.id))
            .expect("the receivers")
    });
    let (csp, d2d, rows, tables) = db
        .read(|c: &Connection| {
            Ok::<_, kourier_db::Error>((
                count(c, "csp_nonce"),
                count(c, "d2d_nonce"),
                count(c, "send_task"),
                count(c, "send_receiver"),
            ))
        })
        .expect("the counts");
    assert_eq!((csp, d2d), kill.nonces, "{what}: the nonce stores");
    assert_eq!(rows, i64::from(kill.state.is_some()), "{what}: send_task");
    if let Some(task) = task {
        check_row(case, kill, task, &receivers, what);
        Stored::keep(&mut stored.message, task.reflect_env.as_ref(), what);
        Stored::keep(&mut stored.update, task.update_env.as_ref(), what);
        stored.id = Some(task.message_id);
    } else {
        assert_eq!(tables, 0, "{what}: receivers of no row");
    }
    for receiver in &receivers {
        if let Some(payload) = &receiver.payload {
            let mut slot = stored.boxes.get(&receiver.identity).cloned();
            Stored::keep(&mut slot, Some(payload), what);
            stored
                .boxes
                .insert(receiver.identity.clone(), payload.clone());
        }
    }

    let m = p.world.mediator();
    let (reflects, sent) = (m.reflects(), m.csp_sent());
    assert_eq!(
        (reflects.len(), sent.len()),
        kill.frames,
        "{what}: the fake's reflects and boxes"
    );
    if let Some(task) = task {
        if let (Some(first), Some(env)) = (reflects.first(), task.reflect_env.as_ref()) {
            assert!(first.bytes == *env, "{what}: the reflect is not the row's");
        }
        if let (Some(last), 2, Some(env)) =
            (reflects.last(), reflects.len(), task.update_env.as_ref())
        {
            assert!(last.bytes == *env, "{what}: the update is not the row's");
        }
    }
    for one in &sent {
        let receiver = receivers
            .iter()
            .find(|r| r.identity.as_bytes() == one.receiver.as_slice());
        if let Some(payload) = receiver.and_then(|r| r.payload.as_ref()) {
            assert!(one.bytes == *payload, "{what}: a box is not the row's");
        }
    }
    p.world.violations().assert_empty();
}

/// The row after a kill, every column that says where it stands.
fn check_row(case: &Case, kill: &Kill, task: &SendTask, receivers: &[SendReceiver], what: &str) {
    let job = case.shape == Shape::Job;
    let (origin, local) = if job {
        (SendOrigin::Job, None)
    } else {
        (SendOrigin::Transmit, Some(LOCAL))
    };
    assert_eq!(task.origin, origin, "{what}: the origin");
    assert_eq!(task.local.as_deref(), local, "{what}: the local");
    if !job {
        assert_eq!(task.message_id, ID, "{what}: the local is the message's id");
    }
    let created_at = if job {
        i64::try_from(START_MS).expect("ms fit i64")
    } else {
        T0
    };
    assert_eq!(task.created_at, created_at, "{what}: the message's time");
    assert_eq!(task.attempts, 0, "{what}: a kill is no failed run");
    assert!(task.has_update, "{what}: a text owes the update");
    let sealed = match task.state {
        SendState::Prepared => (true, false),
        SendState::Acked => (false, true),
        SendState::Reflected | SendState::Done => (false, false),
    };
    assert_eq!(
        (task.reflect_env.is_some(), task.update_env.is_some()),
        sealed,
        "{what}: which sealed columns stand"
    );
    let expected: Vec<(ReceiverState, bool)> = case
        .shape
        .receivers()
        .iter()
        .map(|id| {
            if kill.acked.contains(id) {
                (ReceiverState::Acked, false)
            } else {
                (ReceiverState::Pending, true)
            }
        })
        .collect();
    let got: Vec<(ReceiverState, bool)> = receivers
        .iter()
        .map(|r| (r.state, r.payload.is_some()))
        .collect();
    assert_eq!(got, expected, "{what}: each receiver and its box");
    assert!(
        receivers
            .iter()
            .map(|r| r.identity.as_str())
            .eq(case.shape.receivers().iter().copied()),
        "{what}: the rows are not the receivers"
    );
}

/// The rows of the queue, by what must not differ from a run with no kill: states and counts, no time, no sealed byte.
fn dump(p: &Parent) -> Vec<String> {
    let conn = kourier_db::open_read_only(&db_path(p.dir()), &key()).expect("open the file");
    let mut lines = Vec::new();
    for sql in [
        "SELECT id, origin, kind, local, message_id, conversation, created_at, state, reflect_env IS NULL, \
           update_env IS NULL, has_update, attempts, sent_at IS NULL, finished_at IS NULL \
         FROM send_task ORDER BY id",
        "SELECT task, identity, state, payload IS NULL FROM send_receiver ORDER BY task, identity",
        "SELECT count(*), 'csp' FROM csp_nonce",
        "SELECT count(*), 'd2d' FROM d2d_nonce",
    ] {
        let mut stmt = conn.prepare(sql).expect("prepare");
        let columns = stmt.column_count();
        let got: Vec<String> = stmt
            .query_map([], |r| {
                let mut line = String::new();
                for i in 0..columns {
                    let value: ValueRef<'_> = r.get_ref(i)?;
                    line.push_str(&format!("{value:?}|"));
                }
                Ok(line)
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        lines.extend(got);
    }
    lines
}

/// THE SENTENCE, at the end of a case. THE BYTES FIRST, before anything opens a frame: a flipped bit must meet this sentence and
/// no other rule of the fake.
fn check_frames(p: &Parent, case: &Case, stored: &Stored) {
    let what = case.what;
    let m = p.world.mediator();
    let (reflects, sent) = (m.reflects(), m.csp_sent());
    assert_eq!(
        reflects.len(),
        case.reflects.len(),
        "{what}: the reflects at the end"
    );
    assert_eq!(
        sent.len(),
        case.boxes.iter().map(|(_, n)| n).sum::<usize>(),
        "{what}: the boxes at the end"
    );
    for (purpose, kept) in [(M, &stored.message), (U, &stored.update)] {
        let mut copies = case
            .reflects
            .iter()
            .zip(&reflects)
            .filter(|(p, _)| **p == purpose)
            .map(|(_, one)| &one.bytes);
        let first = copies.next();
        assert!(first.is_some(), "{what}: no {purpose:?} reflect");
        assert!(
            copies.all(|one| Some(one) == first),
            "{what}: a resend of the {purpose:?} reflect is other bytes"
        );
        assert!(
            kept.is_none() || kept.as_ref() == first,
            "{what}: the {purpose:?} reflect is not what the file held"
        );
    }
    for (id, copies) in case.boxes {
        let boxes: Vec<&Vec<u8>> = sent
            .iter()
            .filter(|one| one.receiver.as_slice() == id.as_bytes())
            .map(|one| &one.bytes)
            .collect();
        assert_eq!(boxes.len(), *copies, "{what}: the copies of one receiver");
        assert!(
            boxes.iter().all(|one| *one == boxes[0]),
            "{what}: a resend to a receiver is other bytes"
        );
        if let Some(kept) = stored.boxes.get(*id) {
            assert!(
                boxes[0] == kept,
                "{what}: a receiver's box is not what the file held"
            );
        }
    }
    // At most one frame of a payload per socket: a resend waits for a later one.
    let mut per_socket = BTreeSet::new();
    assert!(
        sent.iter()
            .all(|one| per_socket.insert((one.conn, one.receiver))),
        "{what}: a socket wrote a box twice"
    );
    let mut per_socket = BTreeSet::new();
    assert!(
        reflects
            .iter()
            .all(|one| per_socket.insert((one.conn.0, one.bytes.clone()))),
        "{what}: a socket wrote a reflect twice"
    );
    let lines = p.world.trace().lines();
    let first = |name: &str| {
        lines
            .iter()
            .position(|line| line.split(' ').nth(3) == Some(name))
    };
    let (reflect, boxed) = (first("reflect"), first("csp-outgoing-message"));
    assert!(
        reflect.is_some() && boxed.is_some() && reflect < boxed,
        "{what}: the reflect does not precede the first box"
    );
}

/// What the fake's peers and its phone make of the frames: one message per receiver and copies discarded by their nonce; one
/// `OutgoingMessage` and one `Sent` update with the frames' nonces, a repeat discarded by its D2D nonce (`TP/md.md:31-36`).
fn check_readers(p: &Parent, case: &Case, stored: &Stored) {
    let what = case.what;
    let job = case.shape == Shape::Job;
    let mut nonces = Vec::new();
    let mut id = (!job).then_some(ID).or(stored.id);
    let created_at_ms = if job {
        START_MS
    } else {
        u64::try_from(T0).expect("ms")
    };
    let created_at_s = if job { JOB_S } else { T0_S };
    for (receiver, copies) in case.boxes {
        let opened = p.world.peer(receiver).drain();
        assert_eq!(opened.len(), *copies, "{what}: what a peer read");
        let one = &opened[0];
        assert_eq!(one.discard, None, "{what}: the first is opened");
        assert!(one.sender == ME, "{what}: the sender");
        let message_id = *id.get_or_insert(one.message_id);
        assert_eq!(one.message_id, message_id, "{what}: the message's id");
        assert_eq!(one.created_at_ms, created_at_ms, "{what}: the time (ms)");
        assert_eq!(one.created_at_s, created_at_s, "{what}: the time (s)");
        assert_eq!(one.flags, PUSH, "{what}: the flags");
        assert_eq!(one.ty, case.shape.ty(), "{what}: the type");
        assert!(one.body == case.shape.body(), "{what}: the body differs");
        assert!(
            one.nickname.as_deref() == Some(""),
            "{what}: the nickname differs"
        );
        for copy in &opened[1..] {
            assert_eq!(
                copy.discard,
                Some(Discard::Nonce),
                "{what}: a copy is dropped by its nonce"
            );
            assert!(copy.bytes == one.bytes, "{what}: a copy is other bytes");
        }
        nonces.push(one.nonce.to_vec());
    }
    let id = id.expect("a case has a receiver");

    let (mut messages, mut updates) = (Vec::new(), Vec::new());
    let mut seen = BTreeSet::new();
    let reflects = p.world.mediator().reflects();
    for (one, purpose) in reflects.iter().zip(case.reflects) {
        let content = open_envelope(&p.keys, &one.bytes)
            .and_then(|e| e.content)
            .unwrap_or_else(|| panic!("{what}: the phone cannot open a reflect"));
        match (&content, purpose) {
            (Content::OutgoingMessage(_), M) | (Content::OutgoingMessageUpdate(_), U) => {}
            _ => panic!("{what}: a reflect is not the one at its place"),
        }
        if !seen.insert(one.bytes[..24].to_vec()) {
            continue;
        }
        match content {
            Content::OutgoingMessage(m) => messages.push(m),
            Content::OutgoingMessageUpdate(u) => updates.push(u),
            _ => {}
        }
    }
    assert_eq!(messages.len(), 1, "{what}: what the phone maps");
    assert_eq!(updates.len(), 1, "{what}: the updates the phone maps");
    let message = &messages[0];
    assert!(
        message.conversation == Some(case.shape.conversation()),
        "{what}: the phone's conversation"
    );
    assert_eq!(message.message_id, id, "{what}: the phone's message id");
    assert_eq!(
        message.created_at, created_at_ms,
        "{what}: the phone's time"
    );
    assert_eq!(
        message.r#type,
        i32::from(case.shape.ty()),
        "{what}: the type"
    );
    assert!(
        message.body == case.shape.body(),
        "{what}: the phone's body differs"
    );
    assert!(
        message.nonces == nonces,
        "{what}: OutgoingMessage.nonces is not the boxes' nonces"
    );
    assert!(
        updates[0].updates
            == [Update {
                conversation: Some(case.shape.conversation()),
                message_id: id,
                update: Some(update::Update::Sent(SentMark {})),
            }],
        "{what}: the Sent update differs"
    );
}

/// The file at the end, by its literal times.
fn check_file(p: &Parent, case: &Case) {
    let what = case.what;
    let db = p.open();
    let tasks = db.read(store::send_tasks).expect("the tasks");
    if case.shape == Shape::Job {
        assert_eq!(tasks.len(), 0, "{what}: a job's row goes at its end");
    } else {
        assert_eq!(tasks.len(), 1, "{what}: one row");
        let row = &tasks[0];
        assert_eq!(row.state, SendState::Done, "{what}: the row's state");
        let at_ms = |n: u64| i64::try_from(at(n)).expect("ms");
        assert_eq!(
            row.sent_at,
            Some(at_ms(case.sent_by)),
            "{what}: the update's time"
        );
        assert_eq!(
            row.finished_at,
            Some(at_ms(case.sent_by)),
            "{what}: ZONE DONE's time"
        );
        assert_eq!(
            row.queued_at,
            at_ms(case.prepared_by),
            "{what}: PREPARE's time"
        );
    }
    drop(db);
    assert_eq!(
        p.outbox_rows(),
        0,
        "{what}: a send row states nothing to the core, so a child reaches Online with no core that acks"
    );
}

/// What the child's lines say it was answered.
fn answers(ended: &Ended) -> Vec<String> {
    ended
        .stdout
        .lines()
        .filter_map(|line| line.split_once(ANSWER).map(|(_, said)| said.to_owned()))
        .collect()
}

/// ONE WORLD: every kill and its literal row, the last child, the end. The world (the violations are its last check) and the
/// file's rows.
fn run_case(case: &Case) -> Option<(Parent, Vec<String>)> {
    let what = case.what;
    let p = world(case.shape);
    let mut stored = Stored::default();
    let mut sent_at = T0;
    for one in case.kills {
        hold(&p, case.shape, one.holds);
        let armed = format!("{}:{}", one.point, one.k);
        let ended = spawn(&p, case, Some(&armed), sent_at)?.wait();
        assert!(
            ended.aborted_at_failpoint(),
            "{what} {armed}: the child did not die at its point: {ended:?}\n{}",
            ended.stderr
        );
        let at_kill = format!("{what} {armed}");
        check_kill(&p, case, one, &mut stored, &at_kill);
        // Nothing durable: the core's own retry, with its own time, is a first attempt. A row stands: its time is the row's.
        if one.state.is_some() {
            sent_at = T_RETRY;
        }
        p.advance(STEP);
    }
    hold(&p, case.shape, NOBODY);
    let ended = spawn(&p, case, None, sent_at)?.wait();
    assert!(
        ended.passed(),
        "{what}: the last child failed: {ended:?}\n{}",
        ended.stderr
    );
    check_frames(&p, case, &stored);
    if case.shape != Shape::Job {
        let sent = case.shape.sent(at(case.sent_by));
        assert_eq!(
            answers(&ended),
            [sent.clone(), sent],
            "{what}: the attempt's answer and the ledger's"
        );
    }
    check_readers(&p, case, &stored);
    check_file(&p, case);
    p.world.violations().assert_empty();
    let rows = dump(&p);
    Some((p, rows))
}

/// The run with no kill of `shape`, checked like every other end.
fn reference(shape: Shape) -> Option<Vec<String>> {
    let boxes: &'static [(&'static str, usize)] = match shape {
        Shape::Direct | Shape::Job => &[(ANN, 1)],
        Shape::Group => &[(ANN, 1), (BOB, 1), (CRE, 1)],
    };
    let plain = case("the reference", shape, &[], &[M, U], boxes, 0, 0);
    run_case(&plain).map(|(_, rows)| rows)
}

/// A LOOP: its cases on fresh worlds, then each file against the reference of its shape. The REFERENCE RUNS LAST, so a mutant
/// dies at the window it is named after and not in the one run that has no window. The last world; `None`: no `unshare` and
/// nobody insisted.
fn run_loop(name: &str, cases: &[Case]) -> Option<Parent> {
    let mut ends = Vec::new();
    let mut last = None;
    let mut kills = 0;
    for one in cases {
        let (p, rows) = run_case(one)?;
        kills += one.kills.len();
        ends.push((one, rows));
        last = Some(p);
    }
    let mut references: BTreeMap<Shape, Vec<String>> = BTreeMap::new();
    for (one, rows) in &ends {
        let reference = match references.entry(one.shape) {
            Entry::Occupied(known) => known.into_mut(),
            Entry::Vacant(first) => first.insert(reference(one.shape)?),
        };
        assert!(
            rows == reference,
            "{}: the file is not the no-kill reference's, row for row",
            one.what
        );
    }
    eprintln!(
        "{name}: {kills} kills in {} worlds, {} references",
        cases.len(),
        references.len()
    );
    last
}

/// The child of every `Transmit` case: past the login it asks for the `local` with the time the parent chose, reads the answer,
/// and asks again, which only the ledger can answer. Killed at its failpoint on the way, or it shuts down.
#[tokio::test]
async fn attempt_child() {
    if !support::offline::in_child() {
        return;
    }
    let shape = match std::env::var(SHAPE_ENV)
        .expect("the parent's shape")
        .as_str()
    {
        "direct" => Shape::Direct,
        "group" => Shape::Group,
        other => panic!("a shape: {other}"),
    };
    let sent_at: i64 = std::env::var(SENT_AT_ENV)
        .expect("the parent's time")
        .parse()
        .expect("a time");
    let by_itself = std::env::var(ITSELF_ENV).is_ok_and(|v| v == "1");
    let mut c = piped::child_send_open().await;
    c.send(Command::Connect).await;
    c.wait_state("past the login", |s| {
        matches!(s, ConnState::Syncing { .. } | ConnState::Online { .. })
    })
    .await;
    if by_itself {
        let db = piped::child_file();
        wait_until("the row at done", || {
            db.read(|conn: &Connection| store::send_ledger(conn, LOCAL))
                .expect("the ledger")
                .is_some_and(|row| row.state == SendState::Done)
        })
        .await;
    }
    for _ in 0..2 {
        let (reply, answer) = tokio::sync::oneshot::channel();
        c.send(Command::Transmit {
            local: MessageId(1),
            req: Box::new(shape.request(sent_at)),
            reply,
        })
        .await;
        let outcome = tokio::time::timeout(WALL, answer)
            .await
            .expect("the Transmit is answered in time")
            .expect("the actor keeps the reply");
        println!("{ANSWER}{}", said(&outcome, shape));
    }
    c.finish().await;
}

/// The child of the job cases: it asks for nothing. A job runs from `Online`, by itself, until its row is gone.
#[tokio::test]
async fn job_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut c = piped::child().await;
    c.online().await;
    let db = piped::child_file();
    wait_until("no row", || {
        db.read(store::send_tasks).expect("the tasks").is_empty()
    })
    .await;
    c.finish().await;
}

/// Waits (a wall BOUND, no timer of the test's) until `done` holds of the file.
async fn wait_until(what: &str, done: impl Fn() -> bool) {
    let reached = tokio::time::timeout(WALL, async {
        while !done() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(reached.is_ok(), "the file never showed {what}");
}

/// An answer in a line a failure may print: a receiver by its place in the list, no ID.
fn said(outcome: &TransmitOutcome, shape: Shape) -> String {
    match outcome {
        TransmitOutcome::Sent {
            server_at,
            per_recipient,
        } => {
            let whom: Vec<String> = per_recipient
                .iter()
                .map(|(user, state)| {
                    let place = shape.receivers().iter().position(|id| *id == user.value());
                    match (place, state) {
                        (Some(place), RecipientState::Sent { sealed: false }) => place.to_string(),
                        _ => "another".to_owned(),
                    }
                })
                .collect();
            format!("Sent at {server_at:?} to {}", whom.join(","))
        }
        TransmitOutcome::Partial { delivered, failed } => {
            format!("Partial to {}, failed {}", delivered.len(), failed.len())
        }
        TransmitOutcome::Failed(error) => format!("Failed {:?}", error.kind),
        // Threema gives a message no id of its own (KT-2): never answered so.
        TransmitOutcome::SentAs { .. } => "SentAs".to_owned(),
    }
}

/// W1 and W2: a kill inside ZONE PREPARE leaves nothing and the next attempt is a whole first one; a kill after it leaves the
/// sealed rows with every nonce, no frame has gone, and the next attempt writes exactly those bytes for the first time.
#[test]
fn a_kill_around_the_prepare_leaves_all_or_nothing_and_the_next_attempt_goes_once() {
    const ALL: &[(&str, usize)] = &[(ANN, 1), (BOB, 1), (CRE, 1)];
    const CASES: &[Case] = &[
        case(
            "W1 direct",
            Shape::Direct,
            &[kill(
                fp::INSIDE_SEND_PREPARE,
                1,
                NO_ROW,
                NOBODY,
                (0, 0),
                (0, 0),
            )],
            &[M, U],
            &[(ANN, 1)],
            1,
            1,
        ),
        case(
            "W1 group",
            Shape::Group,
            &[kill(
                fp::INSIDE_SEND_PREPARE,
                1,
                NO_ROW,
                NOBODY,
                (0, 0),
                (0, 0),
            )],
            &[M, U],
            ALL,
            1,
            1,
        ),
        case(
            "W2 direct",
            Shape::Direct,
            &[kill(
                fp::AFTER_SEND_PREPARE,
                1,
                PREPARED,
                NOBODY,
                (1, 1),
                (0, 0),
            )],
            &[M, U],
            &[(ANN, 1)],
            0,
            1,
        ),
        case(
            "W2 group",
            Shape::Group,
            &[kill(
                fp::AFTER_SEND_PREPARE,
                1,
                PREPARED,
                NOBODY,
                (3, 1),
                (0, 0),
            )],
            &[M, U],
            ALL,
            0,
            1,
        ),
    ];
    let Some(last) = run_loop("W1-W2", CASES) else {
        return;
    };
    last.world.violations().assert_empty();
}

/// W3 to W5: while the reflect's ack is not durable the same reflect goes again under a new id, which the phone drops by its
/// nonce; once it is durable no reflect goes and the boxes go for the first time. W5 first: its child must not have written a box.
#[test]
fn a_kill_around_the_reflect_writes_the_same_reflect_and_no_second_message() {
    const ALL: &[(&str, usize)] = &[(ANN, 1), (BOB, 1), (CRE, 1)];
    const CASES: &[Case] = &[
        case(
            "W5 direct",
            Shape::Direct,
            &[kill(
                fp::AFTER_SEND_REFLECTED,
                1,
                REFLECTED,
                NOBODY,
                (1, 1),
                (1, 0),
            )],
            &[M, U],
            &[(ANN, 1)],
            0,
            1,
        ),
        case(
            "W5 group",
            Shape::Group,
            &[kill(
                fp::AFTER_SEND_REFLECTED,
                1,
                REFLECTED,
                NOBODY,
                (3, 1),
                (1, 0),
            )],
            &[M, U],
            ALL,
            0,
            1,
        ),
        case(
            "W3 direct",
            Shape::Direct,
            &[kill(
                fp::AFTER_SEND_REFLECT,
                1,
                PREPARED,
                NOBODY,
                (1, 1),
                (1, 0),
            )],
            &[M, M, U],
            &[(ANN, 1)],
            0,
            1,
        ),
        case(
            "W3 group",
            Shape::Group,
            &[kill(
                fp::AFTER_SEND_REFLECT,
                1,
                PREPARED,
                NOBODY,
                (3, 1),
                (1, 0),
            )],
            &[M, M, U],
            ALL,
            0,
            1,
        ),
        case(
            "W4 direct",
            Shape::Direct,
            &[kill(
                fp::INSIDE_SEND_REFLECTED,
                1,
                PREPARED,
                NOBODY,
                (1, 1),
                (1, 0),
            )],
            &[M, M, U],
            &[(ANN, 1)],
            0,
            1,
        ),
        case(
            "W4 group",
            Shape::Group,
            &[kill(
                fp::INSIDE_SEND_REFLECTED,
                1,
                PREPARED,
                NOBODY,
                (3, 1),
                (1, 0),
            )],
            &[M, M, U],
            ALL,
            0,
            1,
        ),
    ];
    let Some(last) = run_loop("W3-W5", CASES) else {
        return;
    };
    last.world.violations().assert_empty();
}

/// W6 to W8: a box whose ack is not durable goes again with the same bytes, which the peer drops by its nonce; a receiver with a
/// durable ack gets nothing, however many kills follow. The walk holds the acks so that each kill's state is a literal.
#[test]
fn a_kill_around_the_boxes_writes_the_same_bytes_only_to_who_has_no_durable_ack() {
    const ALL: &[(&str, usize)] = &[(ANN, 2), (BOB, 2), (CRE, 2)];
    const CASES: &[Case] = &[
        case(
            "W6 direct",
            Shape::Direct,
            &[kill(
                fp::AFTER_SEND_BOX,
                1,
                REFLECTED,
                NOBODY,
                (1, 1),
                (1, 1),
            )],
            &[M, U],
            &[(ANN, 2)],
            0,
            1,
        ),
        case(
            "W7 direct",
            Shape::Direct,
            &[kill(
                fp::INSIDE_SEND_ACKS,
                1,
                REFLECTED,
                NOBODY,
                (1, 1),
                (1, 1),
            )],
            &[M, U],
            &[(ANN, 2)],
            0,
            1,
        ),
        case(
            "W8 group, one of three acked",
            Shape::Group,
            &[
                kill(fp::AFTER_SEND_ACKS, 1, REFLECTED, &[ANN], (3, 1), (1, 3))
                    .holding(&[BOB, CRE]),
            ],
            &[M, U],
            &[(ANN, 1), (BOB, 2), (CRE, 2)],
            0,
            1,
        ),
        case(
            "W8 and W7 group, a walk of three kills",
            Shape::Group,
            &[
                kill(fp::AFTER_SEND_ACKS, 1, REFLECTED, &[ANN], (3, 1), (1, 3))
                    .holding(&[BOB, CRE]),
                kill(fp::INSIDE_SEND_ACKS, 1, REFLECTED, &[ANN], (3, 1), (1, 5)).holding(&[CRE]),
                kill(
                    fp::AFTER_SEND_ACKS,
                    1,
                    REFLECTED,
                    &[ANN, BOB],
                    (3, 1),
                    (1, 7),
                )
                .holding(&[CRE]),
            ],
            &[M, U],
            &[(ANN, 1), (BOB, 3), (CRE, 4)],
            0,
            3,
        ),
        case(
            "W6 group k=1",
            Shape::Group,
            &[kill(
                fp::AFTER_SEND_BOX,
                1,
                REFLECTED,
                NOBODY,
                (3, 1),
                (1, 1),
            )],
            &[M, U],
            &[(ANN, 2), (BOB, 1), (CRE, 1)],
            0,
            1,
        ),
        case(
            "W6 group k=2",
            Shape::Group,
            &[kill(
                fp::AFTER_SEND_BOX,
                2,
                REFLECTED,
                NOBODY,
                (3, 1),
                (1, 2),
            )],
            &[M, U],
            &[(ANN, 2), (BOB, 2), (CRE, 1)],
            0,
            1,
        ),
        case(
            "W6 group k=3",
            Shape::Group,
            &[kill(
                fp::AFTER_SEND_BOX,
                3,
                REFLECTED,
                NOBODY,
                (3, 1),
                (1, 3),
            )],
            &[M, U],
            ALL,
            0,
            1,
        ),
        case(
            "W7 group",
            Shape::Group,
            &[kill(
                fp::INSIDE_SEND_ACKS,
                1,
                REFLECTED,
                NOBODY,
                (3, 1),
                (1, 3),
            )],
            &[M, U],
            ALL,
            0,
            1,
        ),
    ];
    let Some(last) = run_loop("W6-W8", CASES) else {
        return;
    };
    last.world.violations().assert_empty();
}

/// W9 to W12 under R1: at `acked` only the stored update is owed, and it goes by itself at the next socket before anybody asks;
/// a repeat of it is the same bytes, dropped by the phone; after `done` no frame goes, and the ledger answers with the FIRST
/// time. W12 first: the plainest proof that nothing goes, and it dies by the reply when the ledger is passed by.
#[test]
fn a_kill_around_the_update_lets_it_go_by_itself_and_after_done_nothing_goes() {
    const ALL: &[&str] = &[ANN, BOB, CRE];
    const ONCE: &[(&str, usize)] = &[(ANN, 1), (BOB, 1), (CRE, 1)];
    const CASES: &[Case] = &[
        case(
            "W12 direct",
            Shape::Direct,
            &[kill(fp::AFTER_SEND_DONE, 1, DONE, &[ANN], (1, 2), (2, 1))],
            &[M, U],
            &[(ANN, 1)],
            0,
            0,
        ),
        case(
            "W12 group",
            Shape::Group,
            &[kill(fp::AFTER_SEND_DONE, 1, DONE, ALL, (3, 2), (2, 3))],
            &[M, U],
            ONCE,
            0,
            0,
        ),
        case(
            "W9 direct",
            Shape::Direct,
            &[kill(fp::AFTER_SEND_ACKS, 1, ACKED, &[ANN], (1, 2), (1, 1))],
            &[M, U],
            &[(ANN, 1)],
            0,
            1,
        )
        .by_itself(),
        case(
            "W9 group, a walk of three kills",
            Shape::Group,
            &[
                kill(fp::AFTER_SEND_ACKS, 1, REFLECTED, &[ANN], (3, 1), (1, 3))
                    .holding(&[BOB, CRE]),
                kill(
                    fp::AFTER_SEND_ACKS,
                    1,
                    REFLECTED,
                    &[ANN, BOB],
                    (3, 1),
                    (1, 5),
                )
                .holding(&[CRE]),
                kill(fp::AFTER_SEND_ACKS, 1, ACKED, ALL, (3, 2), (1, 6)),
            ],
            &[M, U],
            &[(ANN, 1), (BOB, 2), (CRE, 3)],
            0,
            3,
        )
        .by_itself(),
        case(
            "W10 direct",
            Shape::Direct,
            &[kill(
                fp::AFTER_SEND_UPDATE,
                1,
                ACKED,
                &[ANN],
                (1, 2),
                (2, 1),
            )],
            &[M, U, U],
            &[(ANN, 1)],
            0,
            1,
        )
        .by_itself(),
        case(
            "W10 group",
            Shape::Group,
            &[kill(fp::AFTER_SEND_UPDATE, 1, ACKED, ALL, (3, 2), (2, 3))],
            &[M, U, U],
            ONCE,
            0,
            1,
        )
        .by_itself(),
        case(
            "W11 direct",
            Shape::Direct,
            &[kill(fp::INSIDE_SEND_DONE, 1, ACKED, &[ANN], (1, 2), (2, 1))],
            &[M, U, U],
            &[(ANN, 1)],
            0,
            1,
        )
        .by_itself(),
        case(
            "W11 group",
            Shape::Group,
            &[kill(fp::INSIDE_SEND_DONE, 1, ACKED, ALL, (3, 2), (2, 3))],
            &[M, U, U],
            ONCE,
            0,
            1,
        )
        .by_itself(),
    ];
    let Some(last) = run_loop("W9-W12", CASES) else {
        return;
    };
    last.world.violations().assert_empty();
}

/// A JOB has no attempt and no reply (§2.4: "by itself at the next `Online`"): each window resumes with nobody asking, W12 is its
/// row gone with nothing more to write. The file holds no `core_outbox` row, so a child whose core never acks reaches `Online`.
#[test]
fn a_job_killed_at_each_window_goes_on_by_itself_at_the_next_online() {
    const ONE: &[(&str, usize)] = &[(ANN, 1)];
    const TWO: &[(&str, usize)] = &[(ANN, 2)];
    const CASES: &[Case] = &[
        case(
            "W3 job",
            Shape::Job,
            &[kill(
                fp::AFTER_SEND_REFLECT,
                1,
                PREPARED,
                NOBODY,
                (1, 1),
                (1, 0),
            )],
            &[M, M, U],
            ONE,
            0,
            0,
        ),
        case(
            "W4 job",
            Shape::Job,
            &[kill(
                fp::INSIDE_SEND_REFLECTED,
                1,
                PREPARED,
                NOBODY,
                (1, 1),
                (1, 0),
            )],
            &[M, M, U],
            ONE,
            0,
            0,
        ),
        case(
            "W5 job",
            Shape::Job,
            &[kill(
                fp::AFTER_SEND_REFLECTED,
                1,
                REFLECTED,
                NOBODY,
                (1, 1),
                (1, 0),
            )],
            &[M, U],
            ONE,
            0,
            0,
        ),
        case(
            "W6 job",
            Shape::Job,
            &[kill(
                fp::AFTER_SEND_BOX,
                1,
                REFLECTED,
                NOBODY,
                (1, 1),
                (1, 1),
            )],
            &[M, U],
            TWO,
            0,
            0,
        ),
        case(
            "W7 job",
            Shape::Job,
            &[kill(
                fp::INSIDE_SEND_ACKS,
                1,
                REFLECTED,
                NOBODY,
                (1, 1),
                (1, 1),
            )],
            &[M, U],
            TWO,
            0,
            0,
        ),
        case(
            "W9 job",
            Shape::Job,
            &[kill(fp::AFTER_SEND_ACKS, 1, ACKED, &[ANN], (1, 2), (1, 1))],
            &[M, U],
            ONE,
            0,
            0,
        ),
        case(
            "W10 job",
            Shape::Job,
            &[kill(
                fp::AFTER_SEND_UPDATE,
                1,
                ACKED,
                &[ANN],
                (1, 2),
                (2, 1),
            )],
            &[M, U, U],
            ONE,
            0,
            0,
        ),
        case(
            "W11 job",
            Shape::Job,
            &[kill(fp::INSIDE_SEND_DONE, 1, ACKED, &[ANN], (1, 2), (2, 1))],
            &[M, U, U],
            ONE,
            0,
            0,
        ),
        case(
            "W12 job",
            Shape::Job,
            &[kill(fp::AFTER_SEND_DONE, 1, NO_ROW, NOBODY, (1, 2), (2, 1))],
            &[M, U],
            ONE,
            0,
            0,
        ),
    ];
    let Some(last) = run_loop("W3-W12 job", CASES) else {
        return;
    };
    last.world.violations().assert_empty();
}
