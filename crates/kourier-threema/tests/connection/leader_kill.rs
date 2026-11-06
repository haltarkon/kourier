//! TH6 L8: THE KILL LOOPS OF THE LEADER RECEIVE and THE RT1 SCENE (brief `briefs/th6.md` §2.5 "THE WINDOWS" and §3's L8
//! row; critic T-M5: the mutant M5 is the one check of the brief's departure from the spec's ack order, P1), through a REAL
//! actor on the parent-resident fake (`support/piped.rs`, the form of `send_kill.rs`): one fresh world per case, a child
//! under `unshare -rn` per socket, a table of kills. The phone's slot exists and the phone is offline, so the mediator
//! promotes Kourier and keeps its reflections for the phone.
//!
//! THE BATCH: a text of ANN (owes a receipt), a group text of BEN in ANN's group, a delivery receipt of ANN. Each case
//! kills one child at one window (§2.5: L1 `INSIDE_INBOUND_RECEIVE`, L2 `AFTER_INBOUND_RECEIVE`, L3 `AFTER_INBOUND_REFLECT:k`,
//! L4 `INSIDE_INBOUND_REFLECTED`, L5 `AFTER_INBOUND_REFLECTED`, L6 `AFTER_INBOUND_ACK:k`), and the next socket is either
//! Kourier as leader again or THE PHONE TAKING OVER (L2a's takeover: Kourier's socket ended, the phone leads and takes what
//! the chat server still queues with `Phone::lead_csp_queue`, then Kourier comes back a follower). AFTER EACH KILL the
//! parent reads the file and the fake: the rows are the state of a PREFIX of the batch (WHAT A LOOP CANNOT DRAW FROM
//! TIMING: how many messages one ZONE RECEIVE takes, and how many reflect-acks one ZONE REFLECTED, are the socket's
//! passes; the file is literally the state of a prefix of `b` received and `r` reflected messages, and the window's own
//! column of §2.5 fixes the rest: TH5a's loop form), every column of a row is its state's, the core's rows are the events
//! of exactly those messages once each, the nonce stores and the receipt jobs are what they make, every reflect Kourier
//! wrote is the bytes the file holds (SEALED ONCE), every CSP ack is of a `reflected` row and comes after its reflect-ack
//! (THE GUARD ORACLE), and the fake saw no violation. At the end every message is in the phone's view exactly once, in
//! Kourier's file once (twice in the core's rows only where the phone's reflection follows Kourier's own receive: the
//! core's O1 drops it), acked once, and every resent reflect is byte-equal to the stored one.
//!
//! THE IMPLICIT CONTACT (§2.5 T1-T4): a text of DAN, whom the directory knows and the file does not: lookup, the
//! CONTACT_SYNC transaction, its commit, then the receive; a kill after the begin, after the transaction's reflect, after
//! the commit, and inside the receive that follows the commit (T4). A child's core never acks (M4), so no job runs in a
//! child: a receipt is a `send_task` row, counted.
//!
//! THE RT1 SCENE (in the in-process harness, a core that acks): the phone offline, 20 messages of every kept kind from three
//! peers, the phone back: each in the phone's view once, in Kourier's file once, every ack after its reflect-ack, the
//! receipts coalesced per sender. Nothing is printed but counts, states and window names.

use std::collections::{BTreeMap, BTreeSet};

use kourier_backend_api::{BackendEvent, Command, decode_outbox_row};
use kourier_db::rusqlite::{Connection, params};
use kourier_threema::failpoints as fp;
use kourier_threema::store::{self, InboundRow, InboundState};
use libthreema::protobuf::d2d::envelope::Content;
use prost::Message as _;

use super::harness::{self, traced};
use super::lines::online_as;
use super::reflect::PHONE;
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, Place, ty};
use crate::support::mock::d2m::ConnId;
use crate::support::mock::envelope::{ContactDelta, GroupDelta};
use crate::support::mock::leader::Recorded;
use crate::support::mock::peer::{Letter, Peer};
use crate::support::mock::phone::{LedOutcome, Phone, open_envelope};
use crate::support::mock::txn::{TxnEnd, TxnSeen};
use crate::support::piped::{self, DEVICE, ME, Mode, Parent, Running, SERVER_GROUP, WALL};
use crate::support::{db_path, key};

/// The peers, synthetic: ANN and BEN stored contacts with real keys, DAN known to the directory only.
const ANN: &str = "L8KANN01";
const BEN: &str = "L8KBEN02";
const CAT: &str = "L8KCAT03";
const DAN: &str = "L8KDAN04";
/// ANN's group, BEN a member.
const GID: u64 = 0x0808_0808_0808_0801;
/// The batch's ids: the text, the group text, the delivery receipt.
const TEXT_ID: u64 = 0x11;
const GROUP_TEXT_ID: u64 = 0x12;
const RECEIPT_ID: u64 = 0x13;
/// DAN's text (T1-T4).
const STRANGER_ID: u64 = 0x21;
/// Every world of these loops is this seed.
const SEED: u64 = 8_801;
/// What one receipt job writes when it is enqueued (PREPARE): the receiver's CSP nonce and its reflect's D2D nonce.
const JOB_NONCES: (usize, usize) = (1, 1);
/// The child of every socket.
const CHILD: &str = "socket_child";

/// A message by its sender and id.
type Key = (String, u64);

fn key_of(sender: &str, id: u64) -> Key {
    (sender.to_owned(), id)
}

/// The batch in arrival order.
fn batch() -> Vec<Key> {
    vec![
        key_of(ANN, TEXT_ID),
        key_of(BEN, GROUP_TEXT_ID),
        key_of(ANN, RECEIPT_ID),
    ]
}

/// What a message of the batch (or DAN's text) leaves for the core: the receipt a `Receipts` event, the rest a message.
fn event_of(key: &Key) -> String {
    if key.1 == RECEIPT_ID {
        "receipts".to_owned()
    } else {
        format!("message {}", body::id_hex(key.1.to_le_bytes()))
    }
}

/// The core's rows of `messages`, each with its multiplicity.
fn events_of(messages: &BTreeMap<Key, usize>) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for (key, n) in messages {
        if *n > 0 {
            *out.entry(event_of(key)).or_default() += *n;
        }
    }
    out
}

/// A world: the two contacts, ANN's group, DAN known to the directory, the phone's slot with the phone offline.
struct World {
    p: Parent,
    phone: Phone,
}

fn world() -> World {
    let p = piped::parent(SEED);
    let db = p.open();
    db.zone(|tx| {
        for id in [ANN, BEN] {
            let peer = p.world.add_identity(id);
            let sync = ContactDelta::create(id, peer.ck.public, START_MS).build();
            tx.execute(
                "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
                params![id, sync.encode_to_vec()],
            )?;
        }
        let sync = GroupDelta::create(ANN, GID, "synthetic group", &[ANN, BEN], START_MS).build();
        tx.execute(
            "INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES (?1, ?2, ?3, 0)",
            params![
                ANN,
                i64::from_ne_bytes(GID.to_ne_bytes()),
                sync.encode_to_vec()
            ],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("seed the contacts and the group");
    drop(db);
    p.world.add_identity(DAN);
    let mut phone = p.world.phone(&p.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    World { p, phone }
}

/// The batch at the chat server (one handle per peer: two handles would draw the same nonces). With `split` ANN's
/// receipt is not posted: ANN's handle comes back for [`post_receipt`].
fn post_batch(p: &Parent, split: bool) -> Option<Peer> {
    let mut ann = p.world.peer(ANN);
    let mut ben = p.world.peer(BEN);
    ann.send(&Letter::text(ME, TEXT_ID, "synthetic text"));
    ben.send(&Letter::new(
        ME,
        GROUP_TEXT_ID,
        ty::GROUP_TEXT,
        &body::group_member_container(ANN, GID, &body::text("synthetic group text")),
    ));
    if split {
        return Some(ann);
    }
    post_receipt(&mut ann);
    None
}

/// ANN's delivery receipt, the batch's third message.
fn post_receipt(ann: &mut Peer) {
    ann.send(&Letter::new(
        ME,
        RECEIPT_ID,
        ty::DELIVERY_RECEIPT,
        &body::delivery_receipt(1, &[[7; 8]]),
    ));
}

/// What the fake saw of Kourier's frames, in order.
#[derive(Default)]
struct Frames {
    /// Each reflected `IncomingMessage`: its message and the sealed bytes.
    reflects: Vec<(Key, Vec<u8>)>,
    /// Each reflected `ContactSync` (a transaction's).
    contact_syncs: usize,
    /// Each CSP `message-ack`.
    acks: Vec<Key>,
}

impl Frames {
    fn reflected(&self) -> BTreeSet<Key> {
        self.reflects.iter().map(|(key, _)| key.clone()).collect()
    }
}

fn frames(p: &Parent) -> Frames {
    let m = p.world.mediator();
    let log = m.leader_log();
    let all = m.reflects();
    let kourier = |conn: u64| m.device_of(ConnId(conn)) == Some(DEVICE);
    let mut out = Frames::default();
    for event in &log.events {
        match event {
            Recorded::Reflect { conn, index, .. } if kourier(*conn) => {
                let seen = &all[*index];
                if seen.flags != 0 {
                    continue;
                }
                match open_envelope(&p.keys, &seen.bytes).and_then(|e| e.content) {
                    Some(Content::IncomingMessage(message)) => out.reflects.push((
                        (message.sender_identity, message.message_id),
                        seen.bytes.clone(),
                    )),
                    Some(Content::ContactSync(_)) => out.contact_syncs += 1,
                    _ => {}
                }
            }
            Recorded::CspAck {
                conn,
                sender,
                message_id,
                ..
            } if kourier(*conn) => out
                .acks
                .push((String::from_utf8_lossy(sender).into_owned(), *message_id)),
            _ => {}
        }
    }
    out
}

/// Kourier's trace lines `dir kind`, over all its connections.
fn kourier_traced(p: &Parent, dir: &str, kind: &str) -> usize {
    let m = p.world.mediator();
    p.world
        .trace()
        .lines()
        .iter()
        .filter(|line| {
            let mut parts = line.split(' ').skip(1);
            let conn = parts.next().and_then(|c| c.parse::<u64>().ok());
            conn.is_some_and(|c| m.device_of(ConnId(c)) == Some(DEVICE))
                && parts.next() == Some(dir)
                && parts.next() == Some(kind)
        })
        .count()
}

/// The file, read by the parent.
struct File {
    rows: Vec<InboundRow>,
    events: BTreeMap<String, usize>,
    csp: usize,
    d2d: usize,
    /// `send_task` rows of kind `receipt`.
    receipts: usize,
    contacts: BTreeSet<String>,
}

fn count(conn: &Connection, sql: &str) -> usize {
    let n: i64 = conn.query_row(sql, [], |r| r.get(0)).expect("a count");
    usize::try_from(n).expect("a count")
}

fn file(p: &Parent) -> File {
    let conn: Connection =
        kourier_db::open_read_only(&db_path(p.dir()), &key()).expect("open the file");
    let rows = store::inbound_rows(&conn).expect("the leader's rows");
    let mut events = BTreeMap::new();
    for sql in [
        "SELECT envelope FROM core_outbox ORDER BY seq",
        "SELECT envelope FROM core_outbox_poison ORDER BY seq",
    ] {
        let mut stmt = conn.prepare(sql).expect("prepare");
        let raw: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        for row in raw {
            for event in decode_outbox_row(&row).expect("a row of events") {
                let name = match &event {
                    BackendEvent::Message(m) => format!("message {}", m.local),
                    BackendEvent::Receipts { .. } => "receipts".to_owned(),
                    BackendEvent::ContactUpsert(_) => "contact".to_owned(),
                    BackendEvent::ConversationUpsert(_) => "conversation".to_owned(),
                    _ => "other".to_owned(),
                };
                *events.entry(name).or_default() += 1;
            }
        }
    }
    let mut stmt = conn
        .prepare("SELECT identity FROM contact")
        .expect("prepare");
    let contacts = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    File {
        rows,
        events,
        csp: count(&conn, "SELECT count(*) FROM csp_nonce"),
        d2d: count(&conn, "SELECT count(*) FROM d2d_nonce"),
        receipts: count(
            &conn,
            "SELECT count(*) FROM send_task WHERE kind = 'receipt'",
        ),
        contacts,
    }
}

/// THE GUARD ORACLE (every CSP ack of a client after the reflect-ack of its message, nothing acked unreflected, one D2D
/// nonce per message) and the fake's violations.
fn oracle(p: &Parent, what: &str) {
    let breaches = p.world.leader_violations(&p.keys, PHONE);
    assert!(
        breaches.is_empty(),
        "{what}: the guard oracle: {breaches:?}"
    );
    p.world.violations().assert_empty();
}

/// A stored reflect may disappear (ZONE REFLECTED NULLs it) but never change: nothing is sealed twice.
fn keep(stored: &mut BTreeMap<Key, Vec<u8>>, key: &Key, bytes: &[u8], what: &str) {
    if let Some(before) = stored.get(key) {
        assert!(
            before.as_slice() == bytes,
            "{what}: the stored reflect changed: sealed again"
        );
    }
    stored.insert(key.clone(), bytes.to_vec());
}

/// What a kill left (the counts the windows judge, and the rows for the takeover's end).
struct Seen {
    /// `b`: the received prefix.
    rows: usize,
    /// `r`: the reflected prefix.
    reflected: usize,
    /// Kourier's reflect frames and acks so far.
    reflects: usize,
    acks: usize,
    /// The messages Kourier reflected at least once.
    by_kourier: BTreeSet<Key>,
    /// The messages Kourier acked to the chat server (each once, each of a `reflected` row: `seen` checks both).
    acked: BTreeSet<Key>,
    /// The rows' messages and states.
    states: Vec<(Key, InboundState)>,
}

/// THE STATE OF A PREFIX: what every kill of the batch must leave (module doc), whatever the window.
fn seen(p: &Parent, stored: &mut BTreeMap<Key, Vec<u8>>, what: &str) -> Seen {
    let f = file(p);
    let k = frames(p);
    let all = batch();
    let keys: Vec<Key> = f
        .rows
        .iter()
        .map(|r| (r.sender.clone(), r.message_id))
        .collect();
    let b = keys.len();
    assert!(
        b <= all.len() && keys[..] == all[..b],
        "{what}: the rows are not a prefix of the batch"
    );
    let r = f
        .rows
        .iter()
        .take_while(|row| row.state == InboundState::Reflected)
        .count();
    for (i, row) in f.rows.iter().enumerate() {
        let pending = row.state == InboundState::Pending;
        assert_eq!(
            pending,
            i >= r,
            "{what}: a pending row before a reflected one"
        );
        assert_eq!(
            (
                row.reflect_env.is_some(),
                row.nonce_hash.is_some(),
                row.reflected_at.is_none()
            ),
            (pending, pending, pending),
            "{what}: the columns of a {:?} row",
            row.state
        );
        assert!(row.ack, "{what}: every message of the batch owes an ack");
        assert_eq!(
            row.receipt,
            row.message_id == TEXT_ID,
            "{what}: only the text owes a receipt"
        );
        if let Some(env) = &row.reflect_env {
            keep(stored, &keys[i], env, what);
        }
    }
    for (key, bytes) in &k.reflects {
        assert!(
            keys.contains(key),
            "{what}: a reflect of a message the file has no row of"
        );
        if let Some(kept) = stored.get(key) {
            assert!(kept == bytes, "{what}: a reflect is not the stored one");
        }
    }
    let by_kourier = k.reflected();
    for key in &keys[..r] {
        assert!(
            by_kourier.contains(key),
            "{what}: a reflected row whose reflect never went"
        );
    }
    let mut acked = BTreeSet::new();
    for key in &k.acks {
        assert!(
            keys[..r].contains(key),
            "{what}: an ack of a message whose row is not reflected"
        );
        assert!(acked.insert(key.clone()), "{what}: a message acked twice");
    }
    let once: BTreeMap<Key, usize> = keys.iter().map(|key| (key.clone(), 1)).collect();
    assert_eq!(
        f.events,
        events_of(&once),
        "{what}: the core's rows are not the events of the received messages, once each"
    );
    // Only the text owes a receipt, and its row is the first: ZONE REFLECTED of it wrote the one job.
    assert_eq!(f.receipts, usize::from(r >= 1), "{what}: the receipt jobs");
    assert_eq!(
        (f.csp, f.d2d),
        (b + f.receipts * JOB_NONCES.0, b + f.receipts * JOB_NONCES.1),
        "{what}: the nonce stores"
    );
    oracle(p, what);
    Seen {
        rows: b,
        reflected: r,
        reflects: k.reflects.len(),
        acks: k.acks.len(),
        by_kourier,
        acked,
        states: keys
            .into_iter()
            .zip(f.rows.iter().map(|row| row.state))
            .collect(),
    }
}

/// The windows of §2.5 for the batch.
#[derive(Clone, Copy, Debug)]
enum W {
    L1,
    L2,
    L3(usize),
    L4,
    L5,
    L6(usize),
}

impl W {
    fn armed(self) -> String {
        match self {
            Self::L1 => format!("{}:1", fp::INSIDE_INBOUND_RECEIVE),
            Self::L2 => format!("{}:1", fp::AFTER_INBOUND_RECEIVE),
            Self::L3(k) => format!("{}:{k}", fp::AFTER_INBOUND_REFLECT),
            Self::L4 => format!("{}:1", fp::INSIDE_INBOUND_REFLECTED),
            Self::L5 => format!("{}:1", fp::AFTER_INBOUND_REFLECTED),
            Self::L6(k) => format!("{}:{k}", fp::AFTER_INBOUND_ACK),
        }
    }

    /// §2.5's "Durable" column of the window, on top of the prefix every kill leaves.
    fn check(self, s: &Seen, what: &str) {
        let (b, r, reflects, acks) = (s.rows, s.reflected, s.reflects, s.acks);
        let holds = match self {
            // Nothing durable.
            Self::L1 => (b, r, reflects, acks) == (0, 0, 0, 0),
            // Pending rows, events, nonces; no frame.
            Self::L2 => b >= 1 && r == 0 && reflects == 0 && acks == 0,
            // As L2, the k-th reflect went; its row is still pending.
            Self::L3(k) => reflects == k && b >= k && r < k && acks <= r,
            // As L2, a reflect went, ZONE REFLECTED rolled back.
            Self::L4 => b >= 1 && r == 0 && reflects >= 1 && acks == 0,
            // `reflected` and the receipt job; no ack yet.
            Self::L5 => r >= 1 && acks == 0,
            // The k-th ack went.
            Self::L6(k) => acks == k && r >= k,
        };
        assert!(
            holds,
            "{what}: the window's state: {b} received, {r} reflected, {reflects} reflects, {acks} acks"
        );
    }
}

fn spawn(p: &Parent, armed: Option<&str>) -> Option<Running> {
    p.spawn(module_path!(), CHILD, Mode::Resume, armed)
}

/// A child killed at `armed` (its point must be hit).
fn kill_at(p: &Parent, armed: &str, what: &str) -> Option<()> {
    let ended = spawn(p, Some(armed))?.wait();
    assert!(
        ended.aborted_at_failpoint(),
        "{what}: the child did not die at its point: {ended:?}\n{}",
        ended.stderr
    );
    Some(())
}

/// The next socket of Kourier, killed by the parent once `done` holds of the fake.
async fn socket_until(p: &Parent, what: &str, done: impl FnMut() -> bool) -> Option<()> {
    let running = spawn(p, None)?;
    p.wait(what, done).await;
    let ended = running.kill();
    assert!(
        ended.killed(),
        "{what}: the child ended by itself: {ended:?}\n{}",
        ended.stderr
    );
    Some(())
}

/// THE NEXT SOCKET AS LEADER, and the end: every message reflected, acked once, in the file once, the phone's view once.
async fn end_as_leader(w: &mut World, stored: &BTreeMap<Key, Vec<u8>>, what: &str) -> Option<()> {
    let p = &w.p;
    let all = batch();
    let m = p.world.mediator().clone();
    socket_until(p, "every message acked", || {
        let acked = frames(p).acks;
        all.iter().all(|key| acked.contains(key)) && m.csp_queue_len(ME) == 0
    })
    .await?;
    let f = file(p);
    let k = frames(p);
    let rows: Vec<(Key, InboundState)> = f
        .rows
        .iter()
        .map(|r| ((r.sender.clone(), r.message_id), r.state))
        .collect();
    let want: Vec<(Key, InboundState)> = all
        .iter()
        .map(|key| (key.clone(), InboundState::Reflected))
        .collect();
    assert_eq!(rows, want, "{what}: the rows at the end");
    assert!(
        f.rows.iter().all(|r| r.reflect_env.is_none()),
        "{what}: a reflected row keeps its bytes"
    );
    let once: BTreeMap<Key, usize> = all.iter().map(|key| (key.clone(), 1)).collect();
    assert_eq!(
        f.events,
        events_of(&once),
        "{what}: the core's rows at the end"
    );
    assert_eq!(f.receipts, 1, "{what}: one receipt job, for the text");
    assert_eq!(
        (f.csp, f.d2d),
        (3 + JOB_NONCES.0, 3 + JOB_NONCES.1),
        "{what}: the nonce stores at the end"
    );
    let mut acks = k.acks.clone();
    acks.sort();
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(acks, sorted, "{what}: each message acked once");
    for key in &all {
        let copies: Vec<&Vec<u8>> = k
            .reflects
            .iter()
            .filter(|(of, _)| of == key)
            .map(|(_, bytes)| bytes)
            .collect();
        assert!(!copies.is_empty(), "{what}: a message never reflected");
        assert!(
            copies.iter().all(|one| *one == copies[0]),
            "{what}: a resent reflect is other bytes"
        );
        if let Some(kept) = stored.get(key) {
            assert!(
                kept == copies[0],
                "{what}: the reflect is not what the file held"
            );
        }
    }
    // The phone back: each message in its view once, the resends discarded by their D2D nonce.
    w.phone.connect();
    let view = w.phone.view();
    assert_eq!(view.incoming_ids(), all, "{what}: the phone's view");
    assert_eq!(
        view.replays,
        k.reflects.len() - all.len(),
        "{what}: the phone's discarded copies"
    );
    oracle(&w.p, what);
    Some(())
}

/// THE PHONE TAKES OVER after the kill, then Kourier comes back a follower; the end.
async fn end_by_takeover(w: &mut World, at_kill: &Seen, what: &str) -> Option<()> {
    let all = batch();
    w.phone.connect();
    assert!(w.phone.leads(), "{what}: the phone does not lead");
    let led = w.phone.lead_csp_queue(ME).expect("the phone leads");
    // It discards by the nonce what Kourier reflected (its view has it) and reflects the rest.
    let got: Vec<(Key, bool)> = led
        .iter()
        .map(|l| {
            (
                (l.sender.clone(), l.message_id),
                matches!(l.outcome, LedOutcome::Reflected { .. }),
            )
        })
        .collect();
    // THE QUEUE AT THE TAKEOVER is what Kourier did not ack before the kill. How many messages that is, is the socket's
    // passes (module doc): mostly one ZONE RECEIVE of 3 (L3:3 then finds no ack), but a batch the child took in two
    // zones (2 + 1 or 1 + 2, seen under load) had the first zone's rows reflected, ZONE REFLECTED committed and acked
    // before the third reflect went, so the chat server dropped them. The claim is the partition: every message of the
    // batch is either still queued for the phone or reflected AND acked by Kourier exactly once (the ack after its
    // reflect-ack: `seen`'s guard oracle), never both, never neither.
    for key in &all {
        let queued = got.iter().any(|(of, _)| of == key);
        let acked = at_kill.acked.contains(key);
        assert!(
            queued != acked,
            "{what}: a message {} at the takeover",
            if queued {
                "both queued and acked"
            } else {
                "neither queued nor acked"
            }
        );
        if acked {
            assert!(
                at_kill
                    .states
                    .iter()
                    .any(|(of, state)| of == key && *state == InboundState::Reflected),
                "{what}: an acked message whose row is not reflected"
            );
        }
    }
    let want: Vec<(Key, bool)> = all
        .iter()
        .filter(|key| !at_kill.acked.contains(*key))
        .map(|key| (key.clone(), !at_kill.by_kourier.contains(key)))
        .collect();
    assert_eq!(
        got, want,
        "{what}: what the leading phone did with the queue"
    );
    assert!(
        led.iter().all(|l| matches!(
            l.outcome,
            LedOutcome::Reflected { .. } | LedOutcome::NonceUsed
        )),
        "{what}: {led:?}"
    );
    let by_phone: BTreeSet<Key> = got
        .iter()
        .filter(|(_, reflected)| *reflected)
        .map(|(key, _)| key.clone())
        .collect();
    let p = &w.p;
    let m = p.world.mediator().clone();
    let (logins, acked) = (
        kourier_traced(p, "in", "csp-login"),
        kourier_traced(p, "in", "reflected-ack"),
    );
    // Kourier back: a follower (the phone leads), it maps the phone's reflections and acks them.
    socket_until(p, "Kourier's follower took the phone's reflections", || {
        kourier_traced(p, "in", "csp-login") > logins
            && kourier_traced(p, "in", "reflected-ack") >= acked + by_phone.len()
    })
    .await?;
    let f = file(p);
    let k = frames(p);
    // C10: the rows of what the phone reflected are gone; what Kourier reflected keeps its row and its state (a pending one
    // stays for the 30-day sweep: the brief's LIMITS).
    let rows: Vec<(Key, InboundState)> = f
        .rows
        .iter()
        .map(|r| ((r.sender.clone(), r.message_id), r.state))
        .collect();
    let kept: Vec<(Key, InboundState)> = at_kill
        .states
        .iter()
        .filter(|(key, _)| !by_phone.contains(key))
        .cloned()
        .collect();
    assert_eq!(rows, kept, "{what}: the rows after the takeover");
    // The core's rows: Kourier's own receive, and the follower's map of the phone's reflection (the core drops the second).
    let received: BTreeSet<Key> = at_kill.states.iter().map(|(key, _)| key.clone()).collect();
    let times: BTreeMap<Key, usize> = all
        .iter()
        .map(|key| {
            (
                key.clone(),
                usize::from(received.contains(key)) + usize::from(by_phone.contains(key)),
            )
        })
        .collect();
    assert!(
        times.values().all(|n| (1..=2).contains(n)),
        "{what}: a message the file never got"
    );
    assert_eq!(
        f.events,
        events_of(&times),
        "{what}: the core's rows after the takeover"
    );
    assert_eq!(
        f.receipts,
        usize::from(at_kill.reflected >= 1),
        "{what}: no receipt from the follower"
    );
    assert_eq!(
        (f.csp, f.d2d),
        (
            all.len() + f.receipts * JOB_NONCES.0,
            at_kill.rows + by_phone.len() + f.receipts * JOB_NONCES.1
        ),
        "{what}: the nonce stores after the takeover"
    );
    assert_eq!(
        (k.reflects.len(), k.acks.len()),
        (at_kill.reflects, at_kill.acks),
        "{what}: the follower reflected or acked"
    );
    assert_eq!(
        m.csp_queue_len(ME),
        0,
        "{what}: the chat server still queues"
    );
    // The phone's view: Kourier's reflections and its own, each message once.
    let view = w.phone.view();
    let mut seen_once: Vec<Key> = view.incoming_ids();
    seen_once.extend(by_phone.iter().cloned());
    seen_once.sort();
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(
        seen_once, sorted,
        "{what}: the phone's view after the takeover"
    );
    oracle(&w.p, what);
    Some(())
}

/// One case: the batch, a kill at `window`, then the next socket. Its world (the violations are the caller's last check).
/// With `split` the batch is FORCED into two zones: the child takes the text and the group text, reflects and acks
/// both, and only then does ANN's receipt reach the chat server (live, to the unblocked leader), so its reflect is the
/// third and a second zone's (what a loaded child may do by itself: `end_by_takeover`).
async fn batch_case(window: Option<W>, takeover: bool, split: bool) -> Option<World> {
    let what = match window {
        Some(w) => format!(
            "{w:?}{}{}",
            if split { " split" } else { "" },
            if takeover { " + takeover" } else { "" }
        ),
        None => "no kill".to_owned(),
    };
    let mut w = world();
    let later = post_batch(&w.p, split);
    let mut stored = BTreeMap::new();
    let at_kill = if let Some(window) = window {
        if let Some(mut ann) = later {
            let running = spawn(&w.p, Some(&window.armed()))?;
            w.p.wait("Kourier's acks of the first two", || {
                frames(&w.p).acks.len() == 2
            })
            .await;
            w.p.world.net().act(|_| post_receipt(&mut ann));
            let ended = running.wait();
            assert!(
                ended.aborted_at_failpoint(),
                "{what}: the child did not die at its point: {ended:?}\n{}",
                ended.stderr
            );
        } else {
            kill_at(&w.p, &window.armed(), &what)?;
        }
        let s = seen(&w.p, &mut stored, &what);
        eprintln!(
            "{what}: {} received, {} reflected, {} reflects, {} acks",
            s.rows, s.reflected, s.reflects, s.acks
        );
        window.check(&s, &what);
        if split {
            assert!(matches!(window, W::L3(3)), "the forced split is L3:3's");
            assert_eq!(
                (s.rows, s.reflected, s.reflects, s.acks),
                (3, 2, 3, 2),
                "{what}: the forced split"
            );
        }
        Some(s)
    } else {
        None
    };
    match (at_kill, takeover) {
        (Some(s), true) => end_by_takeover(&mut w, &s, &what).await?,
        _ => end_as_leader(&mut w, &stored, &what).await?,
    }
    w.p.world.violations().assert_empty();
    Some(w)
}

/// The child of every socket: it connects and runs until its failpoint aborts it or the parent kills it. Its core never
/// acks (M4).
#[tokio::test]
async fn socket_child() {
    if !support::offline::in_child() {
        return;
    }
    let c = piped::child().await;
    c.send(Command::Connect).await;
    c.finish_killed().await;
}

/// L1-L6 for the batch with Kourier leading the next socket. L2 FIRST: MUTANT M5 (the step-0 read after the nonce check:
/// a redelivery of a `pending` row is a used nonce and acked unreflected) must die at its window.
#[tokio::test]
async fn a_kill_at_each_window_of_a_batch_loses_nothing_and_shows_nothing_twice() {
    const WINDOWS: &[W] = &[
        W::L2,
        W::L1,
        W::L3(1),
        W::L3(2),
        W::L3(3),
        W::L4,
        W::L5,
        W::L6(1),
        W::L6(2),
        W::L6(3),
    ];
    let Some(mut last) = batch_case(None, false, false).await else {
        return;
    };
    for window in WINDOWS {
        let Some(w) = batch_case(Some(*window), false, false).await else {
            return;
        };
        last = w;
    }
    eprintln!(
        "leader windows: {} kills in {} worlds",
        WINDOWS.len(),
        WINDOWS.len() + 1
    );
    last.p.world.violations().assert_empty();
}

/// L1-L5 for the batch with THE PHONE TAKING OVER the next socket (§2.5's last column): the phone leads, takes the chat
/// server's queue (what Kourier did not ack before the kill: a batch split over two zones has the first one's messages
/// acked by L3:3), reflects what Kourier did not, discards by the nonce what Kourier reflected; Kourier comes back a
/// follower, its C10 deletes the rows the phone's reflections name, and nothing is shown twice but by the core's O1.
/// Last, L3:3 after a FORCED split (two acked, the third reflected and pending): the queue the phone takes holds the
/// receipt alone, which it discards by the nonce.
#[tokio::test]
async fn a_kill_then_the_phone_taking_over_loses_nothing_and_shows_nothing_twice() {
    const WINDOWS: &[W] = &[W::L2, W::L1, W::L3(1), W::L3(3), W::L4, W::L5];
    for window in WINDOWS {
        if batch_case(Some(*window), true, false).await.is_none() {
            return;
        }
    }
    let Some(last) = batch_case(Some(W::L3(3)), true, true).await else {
        return;
    };
    eprintln!("takeover windows: {} kills", WINDOWS.len() + 1);
    last.p.world.violations().assert_empty();
}

/// T1-T4: the implicit contact's transaction.
#[derive(Clone, Copy, Debug)]
enum T {
    T1,
    T2,
    T3,
    T4,
}

impl T {
    fn armed(self) -> String {
        match self {
            Self::T1 => format!("{}:1", fp::AFTER_TX_BEGIN),
            Self::T2 => format!("{}:1", fp::AFTER_TX_REFLECT),
            Self::T3 => format!("{}:1", fp::AFTER_TX_COMMIT),
            Self::T4 => format!("{}:1", fp::INSIDE_INBOUND_RECEIVE),
        }
    }

    /// The transaction's D2D nonce is durable from its reflect on (written before the frame).
    const fn dead_nonces(self) -> usize {
        match self {
            Self::T1 => 0,
            Self::T2 | Self::T3 | Self::T4 => 1,
        }
    }

    /// The phone got the `ContactSync.Create` of the killed child (released by its commit).
    const fn committed(self) -> bool {
        matches!(self, Self::T3 | Self::T4)
    }
}

/// How Kourier's transactions ended, in order: (committed, released, dropped).
fn kourier_txn_ends(p: &Parent) -> Vec<(bool, usize, usize)> {
    p.world
        .mediator()
        .transactions()
        .into_iter()
        .filter_map(|seen| match seen {
            TxnSeen::Ended {
                device,
                how,
                released,
                dropped,
                ..
            } if device == DEVICE => Some((how == TxnEnd::Committed, released, dropped)),
            _ => None,
        })
        .collect()
}

/// One T case: DAN's text, a kill at `t` (none: the reference), the next socket as leader, the end. The core's rows at the
/// end, for the comparison with the reference, and the world.
async fn stranger_case(t: Option<T>) -> Option<(BTreeMap<String, usize>, World)> {
    let what = t.map_or_else(|| "T no kill".to_owned(), |t| format!("{t:?}"));
    let mut w = world();
    let dan_key = key_of(DAN, STRANGER_ID);
    w.p.world
        .peer(DAN)
        .send(&Letter::text(ME, STRANGER_ID, "synthetic stranger text"));
    let mut ends = Vec::new();
    if let Some(t) = t {
        kill_at(&w.p, &t.armed(), &what)?;
        let f = file(&w.p);
        let k = frames(&w.p);
        assert!(
            !f.contacts.contains(DAN),
            "{what}: the contact before the receive"
        );
        assert!(f.rows.is_empty(), "{what}: a row before the receive");
        assert!(f.events.is_empty(), "{what}: events before the receive");
        assert_eq!(
            (f.csp, f.d2d, f.receipts),
            (0, t.dead_nonces(), 0),
            "{what}: the nonce stores"
        );
        assert_eq!(
            w.p.world.net().directory().asked().len(),
            1,
            "{what}: the lookups"
        );
        assert_eq!(
            (k.contact_syncs, k.reflects.len(), k.acks.len()),
            (usize::from(t.dead_nonces() == 1), 0, 0),
            "{what}: Kourier's frames"
        );
        let end = if t.committed() {
            (true, 1, 0)
        } else {
            (false, 0, t.dead_nonces())
        };
        ends.push(end);
        assert_eq!(
            kourier_txn_ends(&w.p),
            ends,
            "{what}: the transaction's end"
        );
        oracle(&w.p, &what);
    }
    let p = &w.p;
    socket_until(p, "DAN's text acked", || frames(p).acks.contains(&dan_key)).await?;
    ends.push((true, 1, 0));
    let f = file(p);
    let k = frames(p);
    assert!(f.contacts.contains(DAN), "{what}: no contact at the end");
    let rows: Vec<(Key, InboundState)> = f
        .rows
        .iter()
        .map(|r| ((r.sender.clone(), r.message_id), r.state))
        .collect();
    assert_eq!(
        rows,
        [(dan_key.clone(), InboundState::Reflected)],
        "{what}: the row at the end"
    );
    assert_eq!(
        f.events.get(&event_of(&dan_key)),
        Some(&1),
        "{what}: the text in the core's rows"
    );
    assert_eq!(
        f.events.get("contact"),
        Some(&1),
        "{what}: the contact's rows"
    );
    let dead = t.map_or(0, T::dead_nonces);
    assert_eq!(
        (f.csp, f.d2d, f.receipts),
        (1 + JOB_NONCES.0, dead + 2 + JOB_NONCES.1, 1),
        "{what}: the nonce stores and the receipt at the end"
    );
    let kills = usize::from(t.is_some());
    assert_eq!(
        p.world.net().directory().asked().len(),
        1 + kills,
        "{what}: a lookup per socket (the cache dies with the actor)"
    );
    assert_eq!(
        (k.contact_syncs, k.acks.clone()),
        (1 + usize::from(dead == 1), vec![dan_key.clone()]),
        "{what}: Kourier's transactions' reflects and its acks"
    );
    assert_eq!(kourier_txn_ends(p), ends, "{what}: the transactions' ends");
    assert!(
        k.reflects.iter().all(|(key, _)| *key == dan_key) && k.reflects.len() == 1,
        "{what}: the text's reflects"
    );
    let _ = p.world.transaction_scopes(&p.keys);
    // The phone back: the text once; the Create once, or twice when the killed child had committed (T3, T4: the brief's
    // [U] second `Create` of a known contact).
    w.phone.connect();
    let view = w.phone.view();
    assert_eq!(
        view.incoming_ids(),
        std::slice::from_ref(&dan_key),
        "{what}: the phone's view"
    );
    assert_eq!(
        view.contacts().len(),
        1 + usize::from(t.is_some_and(T::committed)),
        "{what}: the Creates the phone took"
    );
    oracle(&w.p, &what);
    Some((f.events, w))
}

/// T1-T4 (§2.5): a kill after the transaction's begin, its reflect and its commit, and inside the receive after the commit;
/// the next socket looks the sender up again and runs a new transaction; the end equals the no-kill reference's rows.
#[tokio::test]
async fn a_kill_in_the_implicit_contacts_transaction_loses_nothing() {
    let Some((reference, mut last)) = stranger_case(None).await else {
        return;
    };
    let kills = [T::T1, T::T2, T::T3, T::T4];
    for t in kills {
        let Some((events, w)) = stranger_case(Some(t)).await else {
            return;
        };
        assert_eq!(
            events, reference,
            "{t:?}: the core's rows are not the reference's"
        );
        last = w;
    }
    eprintln!(
        "transaction windows: {} kills in {} worlds",
        kills.len(),
        kills.len() + 1
    );
    last.p.world.violations().assert_empty();
}

// ---------------------------------------------------------------------------------------------------------------------
// THE RT1 SCENE.

#[test]
fn the_rt1_scene_twenty_messages_of_every_kept_kind_reach_the_phone_and_the_file_once() {
    support::offline::run_child(
        module_path!(),
        "the_rt1_scene_twenty_messages_of_every_kept_kind_reach_the_phone_and_the_file_once_child",
    );
}

/// The scene's letters, interleaved by peer: (sender, type, body, owes a receipt).
fn scene_letters() -> Vec<(&'static str, u8, Vec<u8>, bool)> {
    let group = |inner: &[u8]| body::group_member_container(ANN, GID, inner);
    let here = || body::location("47.3769", "8.5417", None, &Place::Nothing);
    vec![
        (ANN, ty::TEXT, body::text("synthetic one"), true),
        (BEN, ty::TEXT, body::text("synthetic two"), true),
        (CAT, ty::TEXT, body::text("synthetic three"), true),
        (ANN, ty::LOCATION, here(), true),
        (
            BEN,
            ty::GROUP_TEXT,
            group(&body::text("synthetic four")),
            false,
        ),
        (CAT, ty::DEPRECATED_IMAGE, body::filler(40, 1), true),
        (ANN, ty::POLL_SETUP, body::filler(40, 2), true),
        (BEN, ty::GROUP_LOCATION, group(&here()), false),
        (CAT, ty::CALL_ICE_CANDIDATE, body::filler(40, 3), false),
        (ANN, ty::POLL_VOTE, body::filler(40, 4), false),
        (
            BEN,
            ty::GROUP_POLL_SETUP,
            group(&body::filler(40, 5)),
            false,
        ),
        (
            CAT,
            ty::GROUP_REACTION,
            group(&body::reaction_apply([1; 8], "\u{1F44D}".as_bytes())),
            false,
        ),
        (ANN, ty::CALL_OFFER, body::filler(40, 6), false),
        (BEN, ty::CALL_HANGUP, body::filler(40, 7), false),
        (CAT, ty::DELETE_MESSAGE, body::delete_message([2; 8]), false),
        (
            ANN,
            ty::REACTION,
            body::reaction_apply([3; 8], "\u{1F44D}".as_bytes()),
            false,
        ),
        (
            BEN,
            ty::EDIT_MESSAGE,
            body::edit_message([4; 8], "synthetic edit"),
            false,
        ),
        (
            CAT,
            ty::GROUP_DELETE_MESSAGE,
            group(&body::delete_message([5; 8])),
            false,
        ),
        (
            ANN,
            ty::GROUP_TEXT,
            group(&body::text("synthetic five")),
            false,
        ),
        (
            BEN,
            ty::DELIVERY_RECEIPT,
            body::delivery_receipt(2, &[[6; 8]]),
            false,
        ),
    ]
}

/// THE RT1 SCENE (§3's L8 row): the phone offline, 20 messages of every kept kind from three peers (ANN's group with BEN
/// and CAT), Kourier promoted: every message reflected once and acked after its reflect-ack (the oracle in `finish`), in
/// the file once (`reflected`), the core's messages once each, the receipts coalesced per sender (the union of a sender's
/// receipts is its owed ids in order, each once, never more receipts than the owed ids); the phone back views each once.
#[tokio::test]
async fn the_rt1_scene_twenty_messages_of_every_kept_kind_reach_the_phone_and_the_file_once_child()
{
    if !support::offline::in_child() {
        return;
    }
    let s = harness::scene(8_802).with_members(ANN, GID, &[ANN, BEN, CAT]);
    let mut phone = s.world.phone(&s.dgk, harness::SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let mut peers: BTreeMap<&str, _> = [ANN, BEN, CAT]
        .into_iter()
        .map(|id| (id, s.world.peer(id)))
        .collect();
    let mut posted: Vec<Key> = Vec::new();
    let mut owed: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for (n, (from, ty, data, receipt)) in scene_letters().into_iter().enumerate() {
        let id = 0x100 + u64::try_from(n).expect("a small index");
        peers
            .get_mut(from)
            .expect("a peer")
            .send(&Letter::new(harness::ME, id, ty, &data));
        posted.push(key_of(from, id));
        if receipt {
            owed.entry(from).or_default().push(id);
        }
    }
    assert_eq!(posted.len(), 20, "the scene's size");
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    let events = h.ack_until("Online as leader", online_as("leader")).await;
    let world = &h.s.world;
    assert!(
        h.net()
            .wait(
                || traced(world, conn, "in", "csp-incoming-message-ack") >= posted.len(),
                WALL
            )
            .await,
        "not every message was acked"
    );
    assert_eq!(
        traced(world, conn, "in", "csp-incoming-message-ack"),
        posted.len(),
        "an ack too many"
    );
    // Kourier's reflects: one per message, in arrival order.
    let reflected: Vec<Key> = world
        .mediator()
        .reflects()
        .iter()
        .filter(|one| one.conn == conn && one.flags == 0)
        .filter_map(|one| match open_envelope(&h.s.keys, &one.bytes)?.content? {
            Content::IncomingMessage(m) => Some((m.sender_identity, m.message_id)),
            _ => None,
        })
        .collect();
    assert_eq!(reflected, posted, "Kourier's reflects");
    // The file: each message's row once, `reflected`.
    let db = kourier_threema::store::open(&h.db_file(), &key()).expect("open the file");
    let rows = db.read(store::inbound_rows).expect("the rows");
    drop(db);
    let in_file: Vec<(Key, InboundState)> = rows
        .iter()
        .map(|r| ((r.sender.clone(), r.message_id), r.state))
        .collect();
    let want: Vec<(Key, InboundState)> = posted
        .iter()
        .map(|key| (key.clone(), InboundState::Reflected))
        .collect();
    assert_eq!(in_file, want, "the file's rows");
    // The core: no message twice, and only the posted ones.
    let mut locals = BTreeSet::new();
    for event in &events {
        if let BackendEvent::Message(m) = event {
            assert!(
                posted
                    .iter()
                    .any(|(_, id)| body::id_hex(id.to_le_bytes()) == *m.local),
                "a message nobody posted"
            );
            assert!(
                locals.insert(m.local.clone()),
                "a message twice in the core"
            );
        }
    }
    // The receipts: per sender the owed ids in order, each once, coalesced.
    let mut got: BTreeMap<&str, Vec<Vec<u64>>> = BTreeMap::new();
    let ids_in = |got: &BTreeMap<&str, Vec<Vec<u64>>>, id: &str| {
        got.get(id)
            .map_or(0, |r| r.iter().map(Vec::len).sum::<usize>())
    };
    assert!(
        h.net()
            .wait(
                || {
                    for (id, peer) in &mut peers {
                        for one in peer.drain() {
                            assert_eq!(one.ty, ty::DELIVERY_RECEIPT, "a box that is no receipt");
                            assert_eq!(one.body[0], 1, "the receipt's status");
                            let (chunks, rest) = one.body[1..].as_chunks::<8>();
                            assert!(rest.is_empty(), "a receipt body of whole ids");
                            got.entry(*id)
                                .or_default()
                                .push(chunks.iter().map(|c| u64::from_le_bytes(*c)).collect());
                        }
                    }
                    owed.iter().all(|(id, ids)| ids_in(&got, id) >= ids.len())
                },
                WALL
            )
            .await,
        "the receipts did not go"
    );
    for (id, ids) in &owed {
        let receipts = got.get(id).cloned().unwrap_or_default();
        assert_eq!(receipts.concat(), *ids, "a sender's receipts");
        assert!(
            (1..=ids.len()).contains(&receipts.len()),
            "{} receipts for {} ids",
            receipts.len(),
            ids.len()
        );
    }
    // The phone back: each once, in order, no copy.
    phone.connect();
    let view = phone.view();
    assert_eq!(view.incoming_ids(), posted, "the phone's view");
    assert_eq!(view.replays, 0, "a copy reached the phone");
    h.finish().await;
}
