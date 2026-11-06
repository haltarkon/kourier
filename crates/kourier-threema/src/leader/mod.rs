//! THE LEADER'S RECEIVE (TH6, `target/threema/briefs/th6.md` with its critic's changes): Kourier as the device group's
//! leader takes messages from the chat server, maps them through the follower's own mapper, reflects each to the other
//! devices and acks it to the chat server only after its reflect-ack is durable (the M5 order with the nonce in ZONE
//! RECEIVE, premise P1). The one place of `src/` that may name the request for messages (`tests/builder.rs` rule 10),
//! and besides `src/reflect/` the one that may name the two CSP opens `decrypt_message`, `decrypt_metadata` (rule 14).
//!
//! Built so far: `zone` (L1, schema 5): the store of it, ZONE RECEIVE, ZONE REFLECTED, the sweep and the step-0 read.
//! The PURE PARTS (L3): `open` (the head and the two opens with the sender's key), `props` (the incoming columns of the
//! 45 pinned types), `steps` (the receive steps as one verdict per message, with its rule names), `directory` (the
//! predefined contacts' keys, the `fetch_bulk` body and answer, the lookup cache; L5: the call itself), `txn` (L5: the
//! head's wait and the `CONTACT_SYNC` transaction). Two doc-hidden doors let the crate's
//! integration tests check the open against the fake's crypto and the job writers against the fake's builders
//! (`tests/leader.rs`).
//!
//! THE LEADER STAGE (L4b; this file, `seal`, and `conn`'s stage that drives it): ONE [`Leader`] per socket, made when
//! the socket's one unblock went ([`unblock_payload`]: the payload rule 10 keeps in this directory; `conn` writes it
//! once per socket when it leads, is logged in, counted Dry and is not dropping the own device). Every
//! `message-with-metadata-box` joins the FIFO in arrival order; ONE batch of up to [`BATCH`] is in flight at a time:
//! [`plan`] decides each box of the head against a read of the file (STEP 0 first: a `pending` row is RESUMED with its
//! stored reflect, a `reflected` row is ACKED only; then `steps::decide`), the kept and discarded ones go into ONE ZONE
//! RECEIVE, then the reflects (sealed once, `seal`), the reflect-acks, ZONE REFLECTED (with the receipt jobs), and the
//! CSP acks last (the M5 order; THE GUARD: nothing is acked that is neither reflected-and-acked nor a spec discard).
//! A typing indicator is EPHEMERAL (an event to the core and an ephemeral reflect; no zone, no nonce; acked after the
//! reflect only when its flags lack `0x04`, TH6 FIX-A S-L3).
//! THE HEAD'S WAIT (L5, `txn`): a sender the file and the lookup cache do not know is looked up in the DIRECTORY
//! (`directory::lookup`, the socket's loop runs the call), and a contact change of steps 22.4-22.6 runs a `CONTACT_SYNC`
//! transaction; that message waits at the head of the FIFO and the messages behind it wait too ([`plan`] stops the
//! batch there and puts the rest back). A lookup that FAILS (critic T-H3: unreachable, a status, 429 included, an
//! answer that is not the form, no answer in time) HOLDS that message and sets the socket's DIRECTORY-DOWN mark: every
//! later message of an unknown sender on the socket is held without a call ([`RULE_DIRECTORY_DOWN`]), known senders
//! keep flowing, never the counted backoff; the socket is recycled gracefully later (`conn`'s `lead_settle`).
//! HOLDS (no zone, nothing acked; the chat server sends it again at the next socket): the steps' holds (his TH6-Q1
//! "They wait for your phone": a REAL group change `group-sync-needed`, critic T-M1; a group message of a group with a
//! held control message where the steps would discard it, at 22.4.1 too, `group-held-before`, critic T-B1; a type the
//! pinned enum lacks `csp-unknown-type`, C2; the creator's answer that needs a picture `creator-answer-needs-picture`,
//! critic T-M3), an unknown sender while the directory is down ([`RULE_DIRECTORY_DOWN`]), a message whose own zone
//! fails by its content ([`RULE_ZONE_FAILED`], C6) or whose job the full job queue refuses ([`RULE_QUEUE_FULL`], S-L2:
//! that one message, no close, no backoff; T-L6: the batch is redone one message per zone), a box over the FIFO's
//! bound ([`RULE_FIFO_FULL`], critic T-H2: the socket is recycled once the FIFO drained), and a `pending` row whose
//! reflect stood unanswered when two sockets ended ([`RULE_REFLECT_UNANSWERED`], critic T-M4: the actor's memory,
//! [`Shared`]). Not acked and counted too (TH6 FIX-A, S-L1): a row whose ZONE REFLECTED fails
//! ([`RULE_REFLECTED_FAILED`], [`RULE_REFLECTED_QUEUE_FULL`]) and a copy known by its zone ([`RULE_KNOWN`]).
//! THE BLIND HOLDS (TH6 FIX-A, S-B1, [`BLIND_HOLDS`]): a hold taken before the steps saw the message's group (a box
//! over the FIFO, a sender the directory could not be asked for, a type the pinned enum lacks, a head that does not
//! parse) marks the socket, and from then on EVERY group counts as held for the knock-on, so a later message the
//! Common Group Receive Steps or 22.4.1 would discard (and ack) waits too; and once a box was refused for room, every
//! later box of the socket is (THE PREFIX RULE, [`Leader::take`]): nothing overtakes a refused box.
//! THE JOBS (L6, [`jobs`]): the receipts (ZONE REFLECTED, coalesced per sender), the FS `Reject` to a stored sender, the
//! creator's answer and the Common Group Receive Steps' setups, leaves and sync requests (ZONE RECEIVE), each written
//! by `send::enqueue_job` in the zone that makes it true; the two 1 h throttles live in [`Shared`].
//! THE STEP-ASIDE (critic T-H1): a hold under a rule only the phone resolves ([`FOR_THE_PHONE`]) waits for a device that
//! leads, and a running Kourier keeps the lead while its socket stands. So an idle, caught-up leader socket that holds
//! one asks the mediator for the devices (`GetDevicesInfo`, again every [`ASK_AGAIN`] while it holds); when another
//! device shows as connected, the socket closes 1000 ONCE (`Outcome::Yield`: not a failed run, not counted, the next
//! socket after a pause) so the mediator can promote the phone; at most once every [`YIELD_EVERY`] per actor.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use libthreema::common::keys::{ClientKey, PublicKey};
use libthreema::common::{MessageId, ThreemaId};
use libthreema::csp::payload::OutgoingPayload;
use libthreema::protobuf::common::CspE2eMessageType;
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d_sync;
use prost::Message as _;
use zeroize::Zeroizing;

use crate::error::Result;
use crate::reflect::Keys;
use crate::reflect::body::GroupRef;
use crate::send::NewJob;
use crate::send::receivers;
use crate::store::{self, NonceScope};
use steps::{Change, Input, Verdict};
use zone::InboundState;

pub(crate) mod directory;
pub(crate) mod jobs;
#[allow(dead_code, reason = "TH6 L4b's leader stage opens through the steps")]
pub(crate) mod open;
#[allow(
    dead_code,
    reason = "TH6 L4b and L6 read the incoming columns through the steps"
)]
pub(crate) mod props;
pub(crate) mod seal;
#[allow(
    dead_code,
    reason = "a kept message always writes its nonce and reflects: `Keep.nonce` and `Keep.reflect` are read by tests"
)]
pub(crate) mod steps;
pub(crate) mod txn;
pub(crate) mod zone;

/// What [`open_inbound`] read of one payload: plain values, for the integration tests only.
#[doc(hidden)]
pub struct OpenedInbound {
    /// The sender's Threema ID.
    pub sender: String,
    /// The receiver's 8 bytes as text (lossy).
    pub receiver: String,
    /// The head's message id (`u64-le`).
    pub message_id: u64,
    /// The head's created-at (s).
    pub created_at_s: u32,
    /// The CSP flags.
    pub flags: u8,
    /// The metadata's id, time (ms) and nickname, when a metadata box came.
    pub metadata: Option<(u64, u64, Option<String>)>,
    /// The legacy nickname as step 22.2 reads it.
    pub legacy_nickname: Option<String>,
    /// The container's outer type.
    pub outer_type: u8,
    /// The unpadded data after the type.
    pub body: Vec<u8>,
}

impl fmt::Debug for OpenedInbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedInbound")
            .field("flags", &self.flags)
            .field("outer_type", &self.outer_type)
            .field("body", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// DOOR (doc-hidden, the crate's integration tests only; `tests/builder.rs` rule 13): reads and opens `bytes` (a
/// `message-with-metadata-box` payload) as the leader's steps 6-8 do, with the user's client key `own_secret` and the
/// sender's public key, so a test checks [`open`] against the fake's own seal. `Err` is the step's rule name.
///
/// # Errors
///
/// The rule of the step that refuses the payload (`csp-head`, `csp-metadata`, `csp-undecryptable`, `csp-container`,
/// `csp-legacy-ff`), or `csp-undecryptable` for a non-contributory key.
#[doc(hidden)]
pub fn open_inbound(
    own_secret: [u8; 32],
    sender_public: [u8; 32],
    bytes: &[u8],
) -> Result<OpenedInbound, &'static str> {
    let boxed = open::parse(bytes)?;
    let key = ClientKey::from(own_secret)
        .derive_csp_e2e_key(&PublicKey::from(sender_public))
        .ok_or(open::RULE_UNDECRYPTABLE)?;
    let opened = open::open(&key, &boxed)?;
    Ok(OpenedInbound {
        sender: String::from_utf8_lossy(&boxed.sender).into_owned(),
        receiver: String::from_utf8_lossy(&boxed.receiver).into_owned(),
        message_id: boxed.message_id.0,
        created_at_s: boxed.created_at_s,
        flags: boxed.flags,
        metadata: opened
            .metadata
            .map(|m| (m.message_id.0, m.created_at, m.nickname)),
        legacy_nickname: boxed.legacy_nickname(),
        outer_type: opened.outer_type,
        body: opened.data.to_vec(),
    })
}

/// What [`write_body`] writes (the `send::body` writers the leader's jobs use), in plain values.
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub enum BodyOf<'a> {
    /// `delivery-receipt`: the status and the ids in wire order.
    Receipt { status: u8, ids: &'a [[u8; 8]] },
    /// `group-setup`: the members' IDs.
    GroupSetup { members: &'a [&'a str] },
    /// `group-name`.
    GroupName { name: &'a str },
    /// `group-creator-container` around `inner`.
    CreatorContainer { group_id: u64, inner: &'a [u8] },
    /// `group-member-container` around `inner`.
    MemberContainer {
        creator: &'a str,
        group_id: u64,
        inner: &'a [u8],
    },
}

/// DOOR (doc-hidden, the crate's integration tests only; rule 13): the bytes the `send::body` writer of `of` writes,
/// so a test compares them with the fake's builders byte for byte. `None` when an ID is not one.
#[doc(hidden)]
#[must_use]
pub fn write_body(of: BodyOf<'_>) -> Option<Vec<u8>> {
    use crate::send::body;
    Some(match of {
        BodyOf::Receipt { status, ids } => {
            let ids: Vec<MessageId> = ids
                .iter()
                .map(|id| MessageId(u64::from_le_bytes(*id)))
                .collect();
            body::delivery_receipt(status, &ids)
        }
        BodyOf::GroupSetup { members } => {
            let members = members
                .iter()
                .map(|m| ThreemaId::try_from(*m).ok())
                .collect::<Option<Vec<_>>>()?;
            body::group_setup(&members)
        }
        BodyOf::GroupName { name } => body::group_name(name),
        BodyOf::CreatorContainer { group_id, inner } => {
            body::group_creator_container(group_id, inner)
        }
        BodyOf::MemberContainer {
            creator,
            group_id,
            inner,
        } => body::group_member_container(ThreemaId::try_from(creator).ok()?, group_id, inner),
    })
}

// ---- THE LEADER STAGE (L4b) -------------------------------------------------------------------------------------

/// The most messages of one RECEIVE zone and of one batch in flight (the follower's batch).
pub(crate) const BATCH: usize = crate::reflect::BATCH;
/// THE FIFO's bound in boxes (brief §2.6): a box over it is HELD ([`RULE_FIFO_FULL`], critic T-H2), never a close.
pub(crate) const FIFO_BOXES: usize = 256;
/// THE FIFO's bound in bytes.
pub(crate) const FIFO_BYTES: usize = 8 * 1024 * 1024;
/// Critic T-M2: the CSP catch-up ends at `QueueSendComplete`, or when the leader stood idle (nothing in the FIFO, no
/// batch in flight) this long after the unblock without it.
pub(crate) const QUIET_FOR: Duration = Duration::from_secs(10);
/// Critic T-M4: a `pending` row whose reflect stood unanswered when this many sockets ended cools for the actor's life.
const COOL_AT: u32 = 2;

/// T-H2: a box that came while the FIFO was full: HELD (dropped from memory un-acked; the socket is recycled once the
/// FIFO drained, so the server sends it again on the next socket).
pub(crate) const RULE_FIFO_FULL: &str = "fifo-full";
/// T-M4: a `pending` row whose reflect stood unanswered when two sockets ended: HELD for the actor's life.
pub(crate) const RULE_REFLECT_UNANSWERED: &str = "inbound-reflect-unanswered";
/// C6: a message whose own RECEIVE zone fails by its content: HELD (never the follower's probe-and-drop).
pub(crate) const RULE_ZONE_FAILED: &str = "inbound-zone-failed";
/// S-L2: a message whose zone the full job queue refuses: HELD, no close, no backoff.
pub(crate) const RULE_QUEUE_FULL: &str = "inbound-queue-full";
/// TH6 FIX-A, S-L1: a reflected message whose ZONE REFLECTED fails by its content: its row stays `pending`, reflected
/// and NOT acked (the next socket resumes it at step 0). Counted per row.
pub(crate) const RULE_REFLECTED_FAILED: &str = "inbound-reflected-zone-failed";
/// TH6 FIX-A, S-L1: a reflected message whose ZONE REFLECTED the full job queue refuses (its receipt job): its row
/// stays `pending`, NOT acked, no close, no backoff (S-L2's twin). Counted per row.
pub(crate) const RULE_REFLECTED_QUEUE_FULL: &str = "inbound-reflected-queue-full";
/// TH6 FIX-A, S-L1: a kept message whose (sender, id) already had a row when its ZONE RECEIVE ran (a copy of it earlier
/// in the same batch, `zone::Received::Known`): neither reflected nor acked on this socket; the next resumes it at step
/// 0. A copy the server sends twice around a reconnect may show it.
pub(crate) const RULE_KNOWN: &str = "inbound-known";
/// Critic T-H3: the directory could not be asked for an unknown sender (unreachable, a status, 429 included, an answer
/// that is not the form, no answer in time): that message is HELD and so is every later one of an unknown sender on the
/// socket (its DIRECTORY-DOWN mark), without a new call; known senders flow.
pub(crate) const RULE_DIRECTORY_DOWN: &str = "directory-down";
/// T-H3's graceful recycle: a socket whose directory is down ends (`Outcome::Stale`, not counted) once the leader is
/// idle and caught up at least this long after the first such hold, so the next socket asks again (the spec's 429 asks
/// for at least 10 s).
pub(crate) const DIRECTORY_RETRY: Duration = Duration::from_secs(60);
/// T-H3: the latest recycle of such a socket, idle or not (the critic's 10 min).
pub(crate) const DIRECTORY_RECYCLE: Duration = Duration::from_secs(10 * 60);
/// Critic T-H1: the holds only a device that leads can resolve (the phone): a socket that holds one steps aside once
/// the phone shows as connected. Not the holds Kourier resolves itself on a later socket (`fifo-full`,
/// `directory-down`, `inbound-queue-full`, `inbound-reflected-queue-full`, `inbound-known`).
pub(crate) const FOR_THE_PHONE: [&str; 7] = [
    steps::RULE_GROUP_SYNC_NEEDED,
    steps::RULE_GROUP_HELD_BEFORE,
    steps::RULE_CREATOR_PICTURE,
    steps::RULE_UNKNOWN_TYPE,
    RULE_ZONE_FAILED,
    RULE_REFLECTED_FAILED,
    RULE_REFLECT_UNANSWERED,
];
/// TH6 FIX-A, S-B1: the BLIND holds, taken before the steps could name the message's group (the box was not opened, or
/// its type is one the pinned enum lacks, which may be a newer group control type): while one stands on a socket, every
/// group counts as held for the knock-on ([`Leader::mark_blind`]). A head that does not parse and a plan the content
/// fails are blind too (marked where they are held).
pub(crate) const BLIND_HOLDS: [&str; 3] = [
    RULE_FIFO_FULL,
    RULE_DIRECTORY_DOWN,
    steps::RULE_UNKNOWN_TYPE,
];
/// T-H1: while such a hold stands and no other device is connected, the socket asks again this long after the answer.
pub(crate) const ASK_AGAIN: Duration = Duration::from_secs(60);
/// T-H1: the actor steps aside at most once in this span (the critic's 15 min).
pub(crate) const YIELD_EVERY: Duration = Duration::from_secs(15 * 60);

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The CSP payload that asks the chat server to deliver (rule 10: named in this directory only). `conn` writes it once
/// per socket.
pub(crate) fn unblock_payload() -> OutgoingPayload {
    OutgoingPayload::UnblockIncomingMessages
}

/// What the actor keeps of the leader across its sockets (critic T-M4's cooling, L5's lookup cache, L6's throttles and
/// T-H1's last step-aside). In memory: a restart forgets it (and tries a poison row twice more, which the cooling wants).
#[derive(Default)]
pub(crate) struct Shared {
    /// (sender, message id) of a `pending` row: how many sockets ended (a failed run) with its reflect unanswered.
    unanswered: RefCell<HashMap<(String, u64), u32>>,
    /// THE CONTACT LOOKUP CACHE (positive answers only, 30 min).
    cache: RefCell<directory::Cache>,
    /// L6: the two 1 h throttles of the group sync requests ([`jobs::Throttles`]).
    throttles: RefCell<jobs::Throttles>,
    /// T-H1: when the actor last stepped aside (the actor's clock, ms).
    yielded_at: Cell<Option<u64>>,
}

impl Shared {
    /// Whether the row of (`sender`, `message_id`) cools (T-M4).
    fn cooled(&self, sender: &str, message_id: u64) -> bool {
        self.unanswered
            .borrow()
            .get(&(sender.to_owned(), message_id))
            .is_some_and(|n| *n >= COOL_AT)
    }

    /// A socket ended with these rows' reflects unanswered (a failed run of the socket, `Outcome::is_failed_run`).
    pub(crate) fn unanswered(&self, rows: impl IntoIterator<Item = (String, u64)>) {
        let mut map = self.unanswered.borrow_mut();
        for key in rows {
            *map.entry(key).or_default() += 1;
        }
    }

    /// The directory's answer for `identity` at `now_ms` (a valid one is cached).
    pub(crate) fn looked(&self, identity: &str, looked: &directory::Looked, now_ms: u64) {
        self.cache
            .borrow_mut()
            .put(&[(identity.to_owned(), looked.clone())], now_ms);
    }

    /// L6: the zone that wrote the jobs of `throttles` committed at `now_ms`.
    pub(crate) fn throttled(&self, throttles: &[jobs::Throttle], now_ms: u64) {
        if !throttles.is_empty() {
            self.throttles
                .borrow_mut()
                .set(throttles.iter().cloned(), now_ms);
        }
    }

    /// T-H1: the first moment the actor may step aside again ([`YIELD_EVERY`] after the last time; 0 when it never did).
    pub(crate) fn may_yield_from(&self) -> u64 {
        self.yielded_at
            .get()
            .map_or(0, |at| at.saturating_add(ms(YIELD_EVERY)))
    }

    /// T-H1: the actor steps aside at `now_ms`.
    pub(crate) fn yielded(&self, now_ms: u64) {
        self.yielded_at.set(Some(now_ms));
    }
}

/// One message of a batch in flight: its reflect went (or goes) under `reflect_id`, its row is `row`.
#[derive(Clone)]
pub(crate) struct InFlight {
    pub(crate) reflect_id: u32,
    pub(crate) row: i64,
    pub(crate) sender: String,
    pub(crate) message_id: u64,
}

/// THE PER-SOCKET MACHINE (module doc): what the chat server delivered and the leader has not settled, the batch in
/// flight, the socket's held groups (T-B1), and the CSP catch-up (C4, T-M2).
#[derive(Default)]
pub(crate) struct Leader {
    fifo: VecDeque<Vec<u8>>,
    fifo_bytes: usize,
    /// The batch in flight: its reflects wait for their acks.
    pub(crate) in_flight: Vec<InFlight>,
    /// The groups of held control messages (T-B1's set).
    held_groups: HashSet<GroupRef>,
    /// TH6 FIX-A, S-B1: a BLIND hold stands on this socket ([`BLIND_HOLDS`]): every group counts as held (T-B1's
    /// knock-on for all of them), so nothing the Common Group Receive Steps or 22.4.1 would discard is acked behind a
    /// message whose group the steps never saw (holding costs a redelivery, never a loss).
    blind: bool,
    /// Boxes held over the FIFO's bound on this socket (T-H2): the socket is recycled once it drained. While it is above
    /// 0, every later box is held too (S-B1's PREFIX RULE, [`Leader::take`]).
    pub(crate) dropped_for_room: u64,
    /// `QueueSendComplete` came.
    pub(crate) queue_complete: bool,
    /// The CSP backlog is a catch-up (C4): until `QueueSendComplete` (or the quiet fallback) and an empty FIFO.
    pub(crate) catching_up: bool,
    /// T-M2's fallback: when an idle leader counts the end of the catch-up without `QueueSendComplete`.
    pub(crate) quiet_by: Option<u64>,
    /// L5: the message that waits at the head (the directory, a contact transaction); the FIFO waits behind it.
    pub(crate) head: Option<txn::Head>,
    /// Critic T-H3: the directory could not be asked on this socket; when the first message was held for it.
    pub(crate) directory_down: Option<u64>,
    /// L5: a rejected transaction waits for the holder's `TransactionEnded` until then ([`txn::TX_WAIT`]).
    pub(crate) ended_by: Option<u64>,
    /// L6: the throttles the jobs of the plan in hand set (a later message of the same plan sees them; [`Shared`] gets
    /// them once their zone committed).
    planned: HashSet<jobs::Throttle>,
    /// T-H1: the messages this socket held under a rule only the phone resolves ([`FOR_THE_PHONE`]).
    pub(crate) phone_held: u64,
}

impl fmt::Debug for Leader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Leader")
            .field("fifo", &self.fifo.len())
            .field("in_flight", &self.in_flight.len())
            .field("dropped_for_room", &self.dropped_for_room)
            .field("catching_up", &self.catching_up)
            .finish_non_exhaustive()
    }
}

impl Leader {
    /// A message was held by `rule` (the caller counts it in the socket's lines): T-H1 counts the phone's; a blind hold
    /// sets the socket's mark (S-B1).
    pub(crate) fn held(&mut self, rule: &str) {
        if FOR_THE_PHONE.contains(&rule) {
            self.phone_held += 1;
        }
        if BLIND_HOLDS.contains(&rule) {
            self.mark_blind();
        }
    }

    /// TH6 FIX-A, S-B1: a BLIND hold stands on this socket: from now on every group counts as held (the socket's end
    /// clears it; the next socket gets the held box again, first).
    pub(crate) fn mark_blind(&mut self) {
        self.blind = true;
    }

    /// The machine of a socket whose unblock just went: the CSP backlog is a catch-up.
    pub(crate) fn unblocked() -> Self {
        Self {
            catching_up: true,
            ..Self::default()
        }
    }

    /// A box the chat server delivered; `false`: the box is HELD ([`RULE_FIFO_FULL`]): the FIFO is full, or a box was
    /// refused for room on this socket before. THE PREFIX RULE (TH6 FIX-A, S-B1): once one box was refused, so is every
    /// later one, also after a batch drained room, so nothing overtakes a refused box: the socket handles a prefix of
    /// the server's queue (at least 256 boxes) and is recycled once it drained (T-H2). A refusal is a BLIND hold.
    pub(crate) fn take(&mut self, bytes: Vec<u8>) -> bool {
        self.quiet_by = None;
        if self.dropped_for_room > 0
            || self.fifo.len() >= FIFO_BOXES
            || self.fifo_bytes.saturating_add(bytes.len()) > FIFO_BYTES
        {
            self.dropped_for_room += 1;
            self.mark_blind();
            return false;
        }
        self.fifo_bytes = self.fifo_bytes.saturating_add(bytes.len());
        self.fifo.push_back(bytes);
        true
    }

    /// Whether nothing waits: the FIFO is empty, no batch is in flight and no head waits.
    pub(crate) fn idle(&self) -> bool {
        self.fifo.is_empty() && self.in_flight.is_empty() && self.head.is_none()
    }

    /// Whether an answer of the mediator is awaited (a reflect of the batch in flight, a transaction's).
    pub(crate) fn awaits_mediator(&self) -> bool {
        !self.in_flight.is_empty() || self.head.as_ref().is_some_and(txn::Head::awaits_mediator)
    }

    /// Boxes a plan did not reach (behind a head that waits): back to the FIFO's front, in their order.
    fn put_back(&mut self, rest: Vec<Vec<u8>>) {
        for bytes in rest.into_iter().rev() {
            self.fifo_bytes = self.fifo_bytes.saturating_add(bytes.len());
            self.fifo.push_front(bytes);
        }
    }

    /// Critic T-H3: when a socket whose directory is down is recycled, as seen at `now_ms`: [`DIRECTORY_RETRY`] after
    /// the first hold (if the leader is idle and caught up by then), else [`DIRECTORY_RECYCLE`] after it.
    pub(crate) fn recycle_at(&self, now_ms: u64) -> Option<u64> {
        let down = self.directory_down?;
        let retry = down.saturating_add(ms(DIRECTORY_RETRY));
        Some(if now_ms < retry {
            retry
        } else {
            down.saturating_add(ms(DIRECTORY_RECYCLE))
        })
    }

    /// Critic T-H3: whether the socket is recycled now for its directory.
    pub(crate) fn recycle_due(&self, now_ms: u64) -> bool {
        self.directory_down.is_some_and(|down| {
            let since = now_ms.saturating_sub(down);
            (since >= ms(DIRECTORY_RETRY) && self.idle() && !self.catching_up)
                || since >= ms(DIRECTORY_RECYCLE)
        })
    }

    /// The head of the FIFO for the next batch (none while a batch is in flight).
    pub(crate) fn next_batch(&mut self) -> Vec<Vec<u8>> {
        if !self.in_flight.is_empty() {
            return Vec::new();
        }
        let n = self.fifo.len().min(BATCH);
        let batch: Vec<Vec<u8>> = self.fifo.drain(..n).collect();
        self.fifo_bytes = self
            .fifo_bytes
            .saturating_sub(batch.iter().map(Vec::len).sum());
        batch
    }

    /// The in-flight entry of `reflect_id`, taken out.
    pub(crate) fn acked(&mut self, reflect_id: u32) -> Option<InFlight> {
        let at = self
            .in_flight
            .iter()
            .position(|f| f.reflect_id == reflect_id)?;
        Some(self.in_flight.remove(at))
    }
}

/// What the stage does with one box of a batch, in the batch's order.
pub(crate) enum Act {
    /// HELD by `rule` (the count line's `held_by` key and D2's word).
    Hold(&'static str),
    /// STEP 0, a `reflected` row: the ack only (no zone).
    Ack { sender: ThreemaId, message_id: u64 },
    /// STEP 0, a `reflected` row of a message that owes no ack: nothing.
    Nothing,
    /// STEP 0, a `pending` row: its STORED reflect again (never sealed again), then as a kept one.
    Resume {
        row: i64,
        sender: ThreemaId,
        message_id: u64,
        reflect: Zeroizing<Vec<u8>>,
    },
    /// The typing indicator: the event (if the follower's rule states one) and the ephemeral reflect; then the CSP ack
    /// when its flags owe one (TH6 FIX-A, S-L3 = P-L2: no `0x04`; the spec's step 25, Desktop's
    /// `incoming-message.ts:946-956`). An ephemeral reflect is never acked by the mediator, so the ack follows its write.
    Ephemeral {
        event: Option<Box<BackendEvent>>,
        sealed: Vec<u8>,
        ack: Option<(ThreemaId, u64)>,
    },
    /// Into ZONE RECEIVE.
    Zone(Entry),
    /// L5: the message WAITS at the head (the directory, a contact transaction); the plan stopped here.
    Wait(Box<txn::Head>),
    /// L5, a head decided again: its steps ask for this contact change (a transaction begins).
    Contact(Change),
}

impl fmt::Debug for Act {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Hold(rule) => rule,
            Self::Ack { .. } => "Ack",
            Self::Nothing => "Nothing",
            Self::Resume { .. } => "Resume",
            Self::Ephemeral { .. } => "Ephemeral",
            Self::Zone(Entry { keep: None, .. }) => "Discard",
            Self::Zone(Entry { keep: Some(_), .. }) => "Keep",
            Self::Wait(_) => "Wait",
            Self::Contact(_) => "Contact",
        })
    }
}

/// One message of ZONE RECEIVE: a discard (`keep: None`) or a kept one.
pub(crate) struct Entry {
    pub(crate) sender: ThreemaId,
    pub(crate) message_id: u64,
    /// A CSP message-ack is owed (no `0x04`).
    pub(crate) ack: bool,
    /// A discard's CSP nonce, hashed, where the spec marks it (past step 7 with a replay-protected type).
    pub(crate) nonce_hash: Option<[u8; 32]>,
    /// The discard's rule (DEBUG only).
    pub(crate) rule: Option<&'static str>,
    pub(crate) keep: Option<KeptMessage>,
    /// L5: the plaintext `d2d.Envelope { contact_sync }` of the transaction committed for this message: ZONE RECEIVE
    /// applies it first (a kept message's or a discarded one's).
    pub(crate) contact: Option<Zeroizing<Vec<u8>>>,
    /// L6: the jobs its steps send, written in the same zone ([`jobs`]).
    pub(crate) jobs: Vec<NewJob>,
    /// L6: the throttles those jobs set once the zone committed.
    pub(crate) throttles: Vec<jobs::Throttle>,
}

/// What a kept message carries into the zone and out of it.
pub(crate) struct KeptMessage {
    /// The CSP nonce, hashed (written for every kept message).
    pub(crate) nonce_hash: [u8; 32],
    /// The plaintext `d2d.Envelope { incoming_message }` (what the mapper reads; the SAME bytes `sealed` holds).
    pub(crate) plaintext: Zeroizing<Vec<u8>>,
    /// The reflect, sealed once.
    pub(crate) sealed: Vec<u8>,
    pub(crate) d2d_hash: [u8; 32],
    /// T-L3's time.
    pub(crate) server_at: u64,
    pub(crate) flags: u8,
    pub(crate) receipt: bool,
}

/// What the leader's steps read at one plan: the file through `conn`, the actor's memory and the socket's.
struct StoreView<'a> {
    conn: &'a Connection,
    own: &'a str,
    now_ms: u64,
    keys: &'a Keys,
    held_groups: &'a HashSet<GroupRef>,
    /// S-B1: a blind hold stands on the socket: every group counts as held.
    blind: bool,
    planned: &'a HashSet<jobs::Throttle>,
    shared: &'a Shared,
}

impl StoreView<'_> {
    /// L6: whether `throttle` holds: set by this plan, or by a committed zone within the hour.
    fn throttle(&self, throttle: &jobs::Throttle) -> bool {
        self.planned.contains(throttle)
            || self.shared.throttles.borrow().holds(throttle, self.now_ms)
    }
}

impl receivers::View for StoreView<'_> {
    fn contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>> {
        store::load_contact(self.conn, identity)
    }

    fn settings(&self) -> Result<Option<d2d_sync::Settings>> {
        store::load_settings(self.conn)
    }

    fn identities_in_groups(&self) -> Result<HashSet<String>> {
        store::identities_in_groups(self.conn)
    }
}

impl steps::View for StoreView<'_> {
    fn own(&self) -> &str {
        self.own
    }

    fn now_ms(&self) -> u64 {
        self.now_ms
    }

    fn csp_nonce_used(&self, nonce: &libthreema::common::Nonce) -> Result<bool> {
        store::contains_nonce(self.conn, NonceScope::Csp, &self.keys.hash(nonce))
    }

    fn group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>> {
        store::load_group(self.conn, creator, group_id)
    }

    fn cached(&self, identity: &str) -> Option<directory::ContactInit> {
        self.shared
            .cache
            .borrow()
            .get(identity, self.now_ms)
            .cloned()
    }

    fn held_group(&self, group: &GroupRef) -> bool {
        self.blind || self.held_groups.contains(group)
    }

    fn blind(&self) -> bool {
        self.blind
    }

    fn sync_request_throttled(&self, group: &GroupRef) -> bool {
        self.throttle(&jobs::Throttle::SyncRequest(group.clone()))
    }

    fn answer_throttled(&self, group_id: u64, sender: &str) -> bool {
        self.throttle(&jobs::Throttle::Answer(group_id, sender.to_owned()))
    }
}

/// What a plan needs besides the file: the identity's keys and the actor's memory.
pub(crate) struct Planning<'a> {
    pub(crate) keys: &'a Keys,
    pub(crate) ck: &'a ClientKey,
    pub(crate) device: u64,
    pub(crate) now_ms: u64,
    pub(crate) shared: &'a Shared,
}

/// THE PLAN of one batch (module doc), in its order, against a read of the file through `conn`. `Err` only when the
/// store fails (the caller's close).
pub(crate) fn plan(
    conn: &Connection,
    at: &Planning<'_>,
    leader: &mut Leader,
    batch: Vec<Vec<u8>>,
) -> Result<Vec<Act>> {
    let own = at.keys.own();
    let own = own.as_str();
    leader.planned.clear();
    let mut acts = Vec::with_capacity(batch.len());
    let mut boxes = batch.into_iter();
    while let Some(bytes) = boxes.next() {
        match plan_one(conn, at, own, leader, &bytes, None)? {
            Planned::Act(act) => acts.push(act),
            // The head waits: the plan stops, the rest waits behind it.
            Planned::Lookup => {
                acts.push(Act::Wait(Box::new(txn::Head::lookup(bytes))));
                leader.put_back(boxes.collect());
                break;
            }
            // Decided again at once as the head ([`plan_head`]), which then asks for the change.
            Planned::Contact(_) => {
                acts.push(Act::Wait(Box::new(txn::Head::contact(bytes, None))));
                leader.put_back(boxes.collect());
                break;
            }
        }
    }
    Ok(acts)
}

/// THE HEAD DECIDED AGAIN (L5): with the directory's answer and, after a committed transaction, with the change done
/// (`contact_done`). One act: a [`Act::Contact`] when the steps (still, or again) ask for a change.
pub(crate) fn plan_head(
    conn: &Connection,
    at: &Planning<'_>,
    leader: &mut Leader,
    bytes: &[u8],
    looked: Option<&directory::Looked>,
    contact_done: bool,
) -> Result<Act> {
    let own = at.keys.own();
    leader.planned.clear();
    let again = Again {
        looked,
        contact_done,
    };
    Ok(
        match plan_one(conn, at, own.as_str(), leader, bytes, Some(&again))? {
            Planned::Act(act) => act,
            // The answer is in hand, so the steps cannot ask for it again; a belt.
            Planned::Lookup => Act::Hold(RULE_DIRECTORY_DOWN),
            Planned::Contact(change) => Act::Contact(change),
        },
    )
}

/// What a head's second decision knows.
struct Again<'a> {
    looked: Option<&'a directory::Looked>,
    contact_done: bool,
}

/// L6: the jobs of the steps' `jobs` ([`jobs::of_steps`]); `None` when they cannot be written by the content (the caller
/// holds the message, C6); `Err` when the store fails.
fn jobs_of(conn: &Connection, own: &str, wanted: &[steps::Job]) -> Result<Option<Vec<NewJob>>> {
    match jobs::of_steps(conn, own, wanted) {
        Ok(rows) => Ok(Some(rows)),
        Err(error) if error.is_content() => Ok(None),
        Err(error) => Err(error),
    }
}

/// The sender's ID of a `message-with-metadata-box` (the clear head's), for the directory's call.
pub(crate) fn sender_of(bytes: &[u8]) -> Option<String> {
    let head = open::parse(bytes).ok()?;
    ThreemaId::try_from(&head.sender[..])
        .ok()
        .map(|id| id.as_str().to_owned())
}

/// One box's plan: an act, or the head's wait.
enum Planned {
    Act(Act),
    Lookup,
    Contact(Change),
}

fn plan_one(
    conn: &Connection,
    at: &Planning<'_>,
    own: &str,
    leader: &mut Leader,
    bytes: &[u8],
    again: Option<&Again<'_>>,
) -> Result<Planned> {
    let head = open::parse(bytes).ok();
    let sender = head
        .as_ref()
        .and_then(|h| ThreemaId::try_from(&h.sender[..]).ok());
    // STEP 0 (P1): the row of (sender, id) BEFORE the nonce check.
    if let (Some(head), Some(sender)) = (&head, sender) {
        let message_id = head.message_id.0;
        if let Some(row) = zone::inbound_row(conn, sender.as_str(), message_id)? {
            return Ok(Planned::Act(match (row.state, row.reflect_env) {
                (InboundState::Reflected, _) if row.ack => Act::Ack { sender, message_id },
                // A `reflected` row of a message that owes no ack: nothing to do.
                (InboundState::Reflected, _) => Act::Nothing,
                (InboundState::Pending, _) if at.shared.cooled(sender.as_str(), message_id) => {
                    Act::Hold(RULE_REFLECT_UNANSWERED)
                }
                (InboundState::Pending, Some(reflect)) => Act::Resume {
                    row: row.id,
                    sender,
                    message_id,
                    reflect,
                },
                (InboundState::Pending, None) => Act::Hold(RULE_ZONE_FAILED),
            }));
        }
    }
    let view = StoreView {
        conn,
        own,
        now_ms: at.now_ms,
        keys: at.keys,
        held_groups: &leader.held_groups,
        blind: leader.blind,
        planned: &leader.planned,
        shared: at.shared,
    };
    let verdict = steps::decide(
        &view,
        &Input {
            bytes,
            ck: at.ck,
            looked: again.and_then(|a| a.looked),
            contact_done: again.is_some_and(|a| a.contact_done),
        },
    )?;
    let (Some(head), Some(sender)) = (head, sender) else {
        // A head that does not parse is the steps' discard: acked by the head's flags, with no sender to name it is
        // HELD instead (no ack can name it). A BLIND hold (S-B1): nothing of it was seen.
        leader.mark_blind();
        return Ok(Planned::Act(match verdict {
            Verdict::Hold(h) => Act::Hold(h.rule),
            other => Act::Hold(other.rule().unwrap_or(open::RULE_HEAD)),
        }));
    };
    let message_id = head.message_id.0;
    Ok(Planned::Act(match verdict {
        Verdict::Hold(hold) => {
            if let Some(group) = hold.group {
                leader.held_groups.insert(group);
            }
            // S-B1: a type the pinned enum lacks may be a newer group control type the steps cannot place: blind, and
            // marked at once, so the later boxes of this very plan see it.
            if BLIND_HOLDS.contains(&hold.rule) {
                leader.mark_blind();
            }
            Act::Hold(hold.rule)
        }
        // Critic T-H3: while the socket's directory is down, an unknown sender is held without a call.
        Verdict::Lookup if leader.directory_down.is_some() => Act::Hold(RULE_DIRECTORY_DOWN),
        Verdict::Lookup => return Ok(Planned::Lookup),
        Verdict::Contact(change) => return Ok(Planned::Contact(change)),
        // C8: a stored sender's encapsulated FS message: discarded, acked, its nonce written, and the `Reject` as a job
        // of the same zone.
        Verdict::RejectFs(reject) => Act::Zone(Entry {
            sender,
            message_id,
            ack: reject.ack,
            nonce_hash: Some(at.keys.hash(&head.nonce)),
            rule: Some(steps::RULE_FS_REJECTED),
            keep: None,
            contact: None,
            jobs: vec![jobs::fs_reject(&reject)],
            throttles: Vec::new(),
        }),
        Verdict::Discard(d) => {
            if d.rule == steps::RULE_FS_REJECTED {
                // C8's LOSS (critic T-L8): no `Reject` to a sender the file does not hold; its peer never learns.
                tracing::info!(
                    "a forward-security message of a sender who is no contact was discarded without a reject; it is lost for every device"
                );
            }
            let Some(job_rows) = jobs_of(conn, own, &d.jobs)? else {
                return Ok(Planned::Act(Act::Hold(RULE_ZONE_FAILED)));
            };
            let throttles = jobs::throttles_of(&d.jobs, false);
            leader.planned.extend(throttles.iter().cloned());
            Act::Zone(Entry {
                sender,
                message_id,
                ack: d.ack,
                nonce_hash: d.nonce.then(|| at.keys.hash(&head.nonce)),
                rule: Some(d.rule),
                keep: None,
                contact: None,
                jobs: job_rows,
                throttles,
            })
        }
        Verdict::Process(keep) => {
            let Some(job_rows) = jobs_of(conn, own, &keep.jobs)? else {
                return Ok(Planned::Act(Act::Hold(RULE_ZONE_FAILED)));
            };
            // A kept `group-sync-request` sets the sender's answer throttle (its answer or its empty setup).
            let throttles = jobs::throttles_of(
                &keep.jobs,
                keep.outer_type == CspE2eMessageType::GroupSyncRequest,
            );
            let plaintext = seal::plaintext(
                at.device,
                &seal::Incoming {
                    sender: sender.as_str(),
                    message_id,
                    created_at_ms: keep.created_at_ms,
                    outer_type: keep.outer_type as i32,
                    body: &keep.body,
                    nonce: &head.nonce,
                },
                crate::send::encode::draw_envelope_pad(),
            );
            let used = |hash: &[u8; 32]| store::contains_nonce(conn, NonceScope::D2d, hash);
            match seal::seal_once(at.keys, &plaintext, &used) {
                Ok((sealed, d2d_hash)) => {
                    leader.planned.extend(throttles.iter().cloned());
                    Act::Zone(Entry {
                        sender,
                        message_id,
                        ack: keep.ack,
                        nonce_hash: None,
                        rule: None,
                        keep: Some(KeptMessage {
                            nonce_hash: at.keys.hash(&head.nonce),
                            plaintext,
                            sealed,
                            d2d_hash,
                            server_at: keep.created_at_ms,
                            flags: head.flags,
                            receipt: keep.receipt,
                        }),
                        contact: None,
                        jobs: job_rows,
                        throttles,
                    })
                }
                Err(error) if error.is_content() => Act::Hold(RULE_ZONE_FAILED),
                Err(error) => return Err(error),
            }
        }
        Verdict::Ephemeral(e) => {
            let plaintext = seal::plaintext(
                at.device,
                &seal::Incoming {
                    sender: sender.as_str(),
                    message_id,
                    created_at_ms: e.created_at_ms,
                    outer_type: e.outer_type as i32,
                    body: &e.body,
                    nonce: &head.nonce,
                },
                crate::send::encode::draw_envelope_pad(),
            );
            let event = d2d::Envelope::decode(plaintext.as_slice())
                .ok()
                .and_then(|envelope| crate::reflect::typing_of(&envelope));
            match seal::seal_ephemeral(at.keys, &plaintext) {
                Ok(sealed) => Act::Ephemeral {
                    event: event.map(Box::new),
                    sealed,
                    ack: e.ack.then_some((sender, message_id)),
                },
                Err(_) => Act::Hold(RULE_ZONE_FAILED),
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TH6 FIX-A, S-B1 (a): THE PREFIX RULE. The FIFO full (256), the next box is refused; a batch drains room, and the
    /// box after it is refused TOO (nothing overtakes a refused box); each refusal marks the socket blind.
    #[test]
    fn a_box_refused_for_room_holds_every_later_box_of_the_socket() {
        let mut lead = Leader::unblocked();
        for n in 0..FIFO_BOXES {
            assert!(lead.take(vec![u8::try_from(n % 251).expect("a byte"); 64]));
        }
        assert!(!lead.blind, "no blind hold yet");
        assert!(!lead.take(vec![1; 64]), "over the bound");
        assert!(lead.blind, "a refusal for room is a blind hold");
        assert_eq!(lead.next_batch().len(), BATCH, "a batch drained room");
        assert!(
            !lead.take(vec![2; 64]),
            "a box after a refused one was taken"
        );
        assert_eq!(lead.dropped_for_room, 2);
        assert_eq!(lead.fifo.len(), FIFO_BOXES - BATCH, "a refused box joined");
    }

    /// TH6 FIX-A, S-B1: the blind holds mark the socket; a hold whose group the steps named does not.
    #[test]
    fn only_a_blind_hold_marks_the_socket() {
        for rule in [
            steps::RULE_GROUP_SYNC_NEEDED,
            steps::RULE_GROUP_HELD_BEFORE,
            steps::RULE_CREATOR_PICTURE,
            RULE_ZONE_FAILED,
            RULE_QUEUE_FULL,
            RULE_REFLECTED_FAILED,
            RULE_REFLECTED_QUEUE_FULL,
            RULE_KNOWN,
            RULE_REFLECT_UNANSWERED,
        ] {
            let mut lead = Leader::unblocked();
            lead.held(rule);
            assert!(!lead.blind, "{rule} marked the socket");
        }
        for rule in BLIND_HOLDS {
            let mut lead = Leader::unblocked();
            lead.held(rule);
            assert!(lead.blind, "{rule} did not mark the socket");
        }
    }
}
