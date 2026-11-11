//! THE REFLECTION LOOP's batch (TH5 A5a; brief §2.3 as amended by `th5-decisions.md`): every D2M
//! `Reflected` envelope of one `Session::step` pass is OPENED with DGRK (P5 `open_reflected`), its D2D
//! nonce hashed with the own ID (`hash_with`), and the whole pass made durable in ONE zone
//! ([`store::REFLECTED_POINTS`]) before the connection sends a single `reflected-ack`.
//!
//! - EPHEMERAL ones (the Reflected FLAG, decisions C5 and M5) never reach the zone: the connection hands
//!   them to [`open_ephemeral`] (TH5b B3, R12: no zone, no nonce, no ack, no file read; the spec forbids the
//!   ack, `md-d2m.struct.yml`), which opens the box and states an incoming typing indicator as ONE
//!   `Typing`. A typing indicator reflected WITHOUT the flag is an ordinary envelope: stored, acked, and it
//!   states nothing (M5: it waited in a queue).
//! - THE ZONE (C4: at most [`BATCH`] envelopes): per envelope, a D2D nonce already in `d2d_nonce` (or
//!   written earlier in the same zone) is a REPLAY (a redelivery after a kill between the commit and the
//!   ack): acked, nothing new. Otherwise the D2D nonce, the CSP nonces the envelope carries (L1: a
//!   missing or malformed one is skipped, never a reason to drop), and then what the envelope IS
//!   ([`classify`], [`map`]): its events in ONE `core_outbox` row, or in rows of at most 500 events in
//!   order when it states more (TH5b S-L3, `outbox::append_in_rows`; all in this zone, so they are
//!   durable before the ack), or a `deferred` row with the plaintext EXACTLY as decrypted (never a prost
//!   re-encode) under [`classify::MAP_VERSION`], or a counted drop by a rule name (decisions C3, M2, M6).
//!   A CONTACT or GROUP SYNC (A7a, [`sync`]) and a SETTINGS, PROFILE or LIST SYNC (TH5b B4, R13) also write
//!   their BLOB back ([`map::Write`]) in the same transaction, before the next envelope is mapped, so a later
//!   envelope of the batch reads it (a text after a `GroupSync.Create` is mapped; a contact sync after a
//!   settings sync is stated with the new blocked list). The bounds' WARN lines are written once per zone
//!   (`store::warn_dropped`).
//! - DROPPED (C6): a box too short to hold a nonce and a tag, one that does not open, a plaintext that
//!   is no `d2d.Envelope`, or one outside the 1..=65,536 bytes of the `deferred` CHECK: acked, nothing
//!   written, one WARN per rule and batch with the count (the rule's name only).
//! - A ZONE THAT FAILS (H3, round A H-1): the batch again, ONE envelope per zone, and the ERROR'S CODE
//!   decides ([`StoreError::is_content`]). An envelope whose own zone fails with the STORE's code (a full
//!   disk, an I/O error, no memory, a busy, locked, read-only or corrupt file, a code this build does not
//!   know) is never acked or dropped: nothing from there on is acked, the connection ends
//!   ([`Applied::io_failed`]) and the mediator delivers it again. Only a failure that is the CONTENT's (a
//!   constraint, a mismatch, a value too big, a row that cannot be decoded) is probed with a zone of the
//!   D2D nonce alone (a second gate): when that commits, the envelope is acked and dropped (WARN
//!   `zone-failed`); when that fails too, the store fails after all.
//!
//! Nothing here logs a key, an ID, a nonce, a reflect id or a body: rule names and counts only.

use std::rc::Rc;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use kourier_db::{Tx, ZoneDb};
use kourier_model::ConvKey;
use libthreema::common::keys::DeviceGroupKey;
use libthreema::common::{Nonce, ThreemaId};
use libthreema::d2m::payload::{ReflectFlags, Reflected};
use libthreema::protobuf::{d2d, d2d_sync};
use prost::Message as _;
use tokio::sync::Notify;
use zeroize::Zeroizing;

use crate::db;
use crate::error::{Result, StoreError};
use crate::outbox;
use crate::store::{
    self, DeferredBounds, DeferredSource, Dropped, Identity, NewDeferred, NonceScope,
};

pub(crate) mod body;
pub(crate) mod classify;
mod map;
mod notice;
pub(crate) mod remap;
mod sync;

pub(crate) use map::conversation_of;

/// The most reflected envelopes of one zone, and the most datagrams the connection drains for one
/// pass (decisions C4 and H1; the architecture's stage-1 batch).
pub(crate) const BATCH: usize = 30;
/// Step 0 (decision B1): the most bytes of reflected envelopes held in memory before the file is
/// join-ready; over it (or over [`BATCH`] envelopes) the connection closes 1000.
pub(crate) const HOLD_BYTES: usize = 2 * 1024 * 1024;

/// A box shorter than a nonce and a tag cannot open (24 + 16 bytes).
const SEALED_MIN: usize = 24 + 16;
/// The `deferred.envelope` CHECK's upper bound (a 64 KiB reflection).
const ENVELOPE_MAX: usize = 65_536;

/// C6's rule names (the WARN lines; never anything of the envelope).
const RULE_TOO_SHORT: &str = "reflected-too-short";
const RULE_UNDECRYPTABLE: &str = "reflected-undecryptable";
const RULE_UNDECODABLE: &str = "reflected-undecodable";
const RULE_BAD_LENGTH: &str = "reflected-bad-length";
/// H3's rule name: the envelope's own zone failed for a reason that is not the store's.
const RULE_ZONE_FAILED: &str = "zone-failed";

/// The committed zones of this process (a test door: `store::reflected_zones`).
static ZONES: AtomicU64 = AtomicU64::new(0);
/// How many (length, code) pairs the test door holds at once (TH5b R16, N-2: a page with a row that fails by
/// its content and one that fails by the store).
const FAIL_PAIRS: usize = 2;
/// A test door (`store::fail_deferred_of_lengths`): the deferred insert of a plaintext of exactly one of these
/// lengths fails the zone (0: an unused slot), and so does the re-map's zone of a row of that length.
static FAIL_LENGTHS: [AtomicUsize; FAIL_PAIRS] = [AtomicUsize::new(0), AtomicUsize::new(0)];
/// The SQLite extended result code each slot of the door fails with.
static FAIL_CODES: [AtomicI32; FAIL_PAIRS] = [AtomicI32::new(0), AtomicI32::new(0)];

/// How many reflected batch zones (and per-envelope zones of H3) this process committed: the actor
/// tests count the zones of a pass with it. Nothing in the application calls it.
#[doc(hidden)]
#[must_use]
pub fn reflected_zones() -> u64 {
    ZONES.load(Ordering::SeqCst)
}

/// Makes the deferred insert of every reflected plaintext of exactly `length` bytes fail its zone with
/// the SQLite extended result code `code` (`length` 0 turns it off), and the re-map's zone of every
/// deferred row of that length too: the actor tests' way to H3's "a zone that fails the same way every
/// time" for BOTH kinds of failure (a content code such as 275, `SQLITE_CONSTRAINT_CHECK`, or the
/// store's such as 13, `SQLITE_FULL`). One pair: [`fail_deferred_of_lengths`] with `[(length, code)]`.
/// Nothing in the application calls it.
#[doc(hidden)]
pub fn fail_deferred_of_length(length: usize, code: i32) {
    fail_deferred_of_lengths(&[(length, code)]);
}

/// The same door with up to two `(length, code)` pairs at once (TH5b R16, N-2): a plaintext of each length
/// fails with its own code, so ONE re-map page can hold a row that fails by its content (275) and one that
/// fails by the store (13). An empty slice (or length 0) turns the door off; every call replaces the pairs
/// of the one before. Nothing in the application calls it.
///
/// # Panics
///
/// With more than two pairs (a test's mistake).
#[doc(hidden)]
pub fn fail_deferred_of_lengths(pairs: &[(usize, i32)]) {
    assert!(
        pairs.len() <= FAIL_PAIRS,
        "the door holds at most two (length, code) pairs"
    );
    for (i, (length, code)) in FAIL_LENGTHS.iter().zip(&FAIL_CODES).enumerate() {
        let (fail, with) = pairs.get(i).copied().unwrap_or((0, 0));
        // The code first: a reader that sees the length sees its code.
        code.store(with, Ordering::SeqCst);
        length.store(fail, Ordering::SeqCst);
    }
}

/// The door's failure of `op` for a plaintext of `length` bytes, if a pair is set for that length.
fn door_error(length: usize, op: &'static str) -> Option<StoreError> {
    FAIL_LENGTHS
        .iter()
        .zip(&FAIL_CODES)
        .find(|(fail, _)| {
            let fail = fail.load(Ordering::SeqCst);
            fail != 0 && fail == length
        })
        .map(|(_, code)| StoreError::Sql {
            op,
            code: Some(code.load(Ordering::SeqCst)),
        })
}

/// TH5b R1: the conversation a STORED `deferred` plaintext belongs to (the clear's question,
/// `store::clear_deferred`): the `d2d.Envelope` decoded (no key: the rows are plaintext) and placed by
/// [`conversation_of`]; a row that does not decode belongs to none (kept).
pub(crate) fn row_conversation(plaintext: &[u8]) -> Option<ConvKey> {
    d2d::Envelope::decode(plaintext)
        .ok()
        .and_then(|envelope| conversation_of(&envelope))
}

/// Whether the mediator marked `reflected` EPHEMERAL (decisions C5, M5: no zone, no nonce, no ack).
pub(crate) fn is_ephemeral(reflected: &Reflected) -> bool {
    reflected.flags.0 & ReflectFlags::EPHEMERAL_MARKER != 0
}

/// R12, AN EPHEMERAL ENVELOPE (its flag; the connection never stores, hashes or acks one): opened with DGRK
/// and decoded, then [`map::typing`] says what it states. `Some` only for an incoming typing indicator from a
/// Threema ID with ONE byte 0 or 1; anything else is `None` with a DEBUG line naming its rule (a box that does
/// not open or decode by C6's names, `typing-byte`, `typing-sender`, `ephemeral-kind`). Nothing of the
/// envelope is logged, and nothing is kept: the plaintext is zeroed on drop.
pub(crate) fn open_ephemeral(keys: &Keys, reflected: &Reflected) -> Option<BackendEvent> {
    let rule = if reflected.envelope.len() < SEALED_MIN {
        RULE_TOO_SHORT
    } else {
        match keys.dgk.open_reflected(&reflected.envelope) {
            Err(_) => RULE_UNDECRYPTABLE,
            Ok((_nonce, plaintext)) => {
                let plaintext = Zeroizing::new(plaintext);
                match d2d::Envelope::decode(plaintext.as_slice()) {
                    Err(_) => RULE_UNDECODABLE,
                    Ok(envelope) => match map::typing(&envelope) {
                        Ok(event) => return Some(event),
                        Err(rule) => rule,
                    },
                }
            }
        }
    };
    tracing::debug!(rule, "an ephemeral reflected envelope states nothing");
    None
}

/// TH6 L4b: what an incoming typing indicator the LEADER took from the chat server states (the follower's own rule,
/// [`map::typing`] over the `IncomingMessage` the leader reflects ephemerally): `Some` only for a typing indicator from
/// a Threema ID with ONE byte 0 or 1; else `None` with a DEBUG line naming its rule.
pub(crate) fn typing_of(envelope: &d2d::Envelope) -> Option<BackendEvent> {
    match map::typing(envelope) {
        Ok(event) => Some(event),
        Err(rule) => {
            tracing::debug!(rule, "an incoming typing indicator states nothing");
            None
        }
    }
}

/// What opens and hashes a connection's reflected envelopes: DGK (DGRK is derived from it) and the
/// own ID (the HMAC key of the nonce stores).
pub(crate) struct Keys {
    dgk: DeviceGroupKey,
    own: ThreemaId,
}

impl Keys {
    /// The own Threema ID.
    pub(crate) fn own(&self) -> ThreemaId {
        self.own
    }

    /// DGK: the send core seals its envelopes with it (only `src/send/encode.rs` names the seal, rule 14).
    pub(crate) fn dgk(&self) -> &DeviceGroupKey {
        &self.dgk
    }

    /// A nonce in the form both nonce stores keep: HMAC-SHA256 under the own ID (`hash_with`). The ONE place the
    /// rule is applied for the reflection loop and for the send core (rule 14: `src/send/` never names `hash_with`).
    pub(crate) fn hash(&self, nonce: &Nonce) -> [u8; 32] {
        nonce.hash_with(self.own).0
    }

    /// TH6 L5: an encoded `d2d.TransactionScope` sealed with DGTSK and a random nonce ahead (`BeginTransaction`'s
    /// `encrypted_scope`). Here because rule 14 keeps every P5 seal in this directory; `None` when the cipher refuses.
    pub(crate) fn seal_scope(&self, scope: Vec<u8>) -> Option<Vec<u8>> {
        self.dgk.seal_transaction_scope(scope).ok()
    }

    /// The keys of `dgk` and `own`, for the crate's unit tests.
    #[cfg(test)]
    pub(crate) fn for_tests(dgk: DeviceGroupKey, own: ThreemaId) -> Self {
        Self { dgk, own }
    }

    /// The keys of the stored identity; `Err` names the field that does not convert.
    pub(crate) fn of(identity: &Identity) -> Result<Self, &'static str> {
        let own =
            ThreemaId::try_from(identity.threema_id.as_str()).map_err(|_| "the identity's ID")?;
        Ok(Self {
            dgk: DeviceGroupKey::from(*identity.device_group_key),
            own,
        })
    }
}

/// One reflected envelope, opened (or the rule that drops it).
struct Entry {
    reflect_id: u32,
    server_at: u64,
    flags: u16,
    body: Body,
}

enum Body {
    /// C6: acked, nothing written.
    Dropped(&'static str),
    Opened {
        /// The D2D nonce hashed with the own ID.
        hash: [u8; 32],
        /// The CSP nonces the envelope carries, hashed (L1: malformed ones skipped).
        csp: Vec<[u8; 32]>,
        /// The `d2d.Envelope` exactly as decrypted (padding and unknown fields kept).
        plaintext: Zeroizing<Vec<u8>>,
        /// The same bytes decoded: the map reads it, so it is decoded once.
        envelope: Box<d2d::Envelope>,
    },
}

/// The opened envelopes of one zone, in arrival order (at most [`BATCH`]).
pub(crate) struct Batch(Vec<Entry>);

impl Batch {
    /// Opens and hashes `reflected` (never an ephemeral one: the connection skips those).
    pub(crate) fn open(keys: &Keys, reflected: Vec<Reflected>) -> Self {
        Self(reflected.into_iter().map(|r| open_one(keys, r)).collect())
    }
}

fn open_one(keys: &Keys, reflected: Reflected) -> Entry {
    let body = if reflected.envelope.len() < SEALED_MIN {
        Body::Dropped(RULE_TOO_SHORT)
    } else {
        match keys.dgk.open_reflected(&reflected.envelope) {
            Err(_) => Body::Dropped(RULE_UNDECRYPTABLE),
            Ok((nonce, plaintext)) => decoded(keys, &nonce, Zeroizing::new(plaintext)),
        }
    };
    Entry {
        reflect_id: reflected.reflect_id,
        server_at: reflected.timestamp,
        flags: reflected.flags.0,
        body,
    }
}

fn decoded(keys: &Keys, nonce: &Nonce, plaintext: Zeroizing<Vec<u8>>) -> Body {
    if plaintext.is_empty() || plaintext.len() > ENVELOPE_MAX {
        return Body::Dropped(RULE_BAD_LENGTH);
    }
    let Ok(envelope) = d2d::Envelope::decode(plaintext.as_slice()) else {
        return Body::Dropped(RULE_UNDECODABLE);
    };
    Body::Opened {
        hash: keys.hash(nonce),
        csp: csp_nonces(&envelope, keys),
        plaintext,
        envelope: Box::new(envelope),
    }
}

/// The CSP nonces of a reflected message (`IncomingMessage.nonce`, `OutgoingMessage.nonces`, "optional
/// for now", `md-d2d.proto`), hashed with the own ID; one that is not 24 bytes is skipped (L1).
fn csp_nonces(envelope: &d2d::Envelope, keys: &Keys) -> Vec<[u8; 32]> {
    use d2d::envelope::Content;
    let raw: Vec<&[u8]> = match &envelope.content {
        Some(Content::IncomingMessage(m)) => vec![m.nonce.as_slice()],
        Some(Content::OutgoingMessage(m)) => m.nonces.iter().map(Vec::as_slice).collect(),
        _ => Vec::new(),
    };
    raw.into_iter()
        .filter_map(|n| Nonce::try_from(n).ok())
        .map(|n| keys.hash(&n))
        .collect()
}

/// What a zone's owner needs to run it.
pub(crate) struct Ctx<'a> {
    pub(crate) db: &'a ZoneDb,
    /// The local time of the zone (ms; `deferred.received_at`, a message's `received_at`).
    pub(crate) now_ms: i64,
    pub(crate) bounds: DeferredBounds,
    /// The own Threema ID: which mention is ours.
    pub(crate) own: ThreemaId,
    /// Wakes the forwarder once a zone with events committed (`outbox::append_events`).
    pub(crate) wake: &'a Rc<Notify>,
    /// Told once a batch with a contact or group sync committed: a profile picture may differ now. `None` for the
    /// messages a leader takes from the chat server, which are never a sync.
    pub(crate) pictures: Option<&'a Notify>,
    /// The batch came before the connection's Dry (a catch-up message, not announced one by one).
    pub(crate) catch_up: bool,
}

/// What became of a batch: the ids to ack, in arrival order, each only after its zone committed.
pub(crate) struct Applied {
    pub(crate) acks: Vec<u32>,
    /// The store failed (H3, H-1: a failure with the store's code, or a probe that did not commit): the
    /// ids after `acks` are NOT acked, the connection ends and the mediator delivers them again.
    pub(crate) io_failed: bool,
}

/// THE ZONE of one batch (module doc), with H3's retry. Returns only after every commit it made.
pub(crate) fn apply(ctx: &Ctx<'_>, batch: Batch) -> Applied {
    let entries = batch.0;
    let mut warned = Counts::default();
    let opened = entries
        .iter()
        .any(|e| matches!(e.body, Body::Opened { .. }));
    let first = if opened {
        zone(ctx, &entries)
    } else {
        Ok(Counts::default())
    };
    if let Ok(mapped) = first {
        warned.merge(mapped);
        for entry in &entries {
            if let Body::Dropped(rule) = entry.body {
                warned.add(rule);
            }
        }
        warned.warn();
        return Applied {
            acks: entries.iter().map(|e| e.reflect_id).collect(),
            io_failed: false,
        };
    }
    // H3: ONE envelope per zone.
    tracing::warn!("a batch of reflected envelopes was not stored; one envelope per zone");
    let mut acks = Vec::with_capacity(entries.len());
    for entry in &entries {
        match &entry.body {
            Body::Dropped(rule) => warned.add(rule),
            Body::Opened { hash, .. } => match zone(ctx, std::slice::from_ref(entry)) {
                Ok(mapped) => warned.merge(mapped),
                Err(error) => {
                    // H-1: the CODE decides. The store's failure is never the envelope's: not acked,
                    // not dropped. A content failure is probed (the nonce alone) as a second gate.
                    if !error.is_content() || nonce_alone(ctx.db, hash).is_err() {
                        tracing::warn!(
                            "the store fails; the rest of the reflected batch is not acked"
                        );
                        warned.warn();
                        return Applied {
                            acks,
                            io_failed: true,
                        };
                    }
                    warned.add(RULE_ZONE_FAILED);
                }
            },
        }
        acks.push(entry.reflect_id);
    }
    warned.warn();
    Applied {
        acks,
        io_failed: false,
    }
}

/// ONE zone over `entries` (the REFLECTED kill points); the rules of the envelopes it dropped.
fn zone(ctx: &Ctx<'_>, entries: &[Entry]) -> Result<Counts> {
    let mut synced = false;
    let counts = db::zone(ctx.db, store::REFLECTED_POINTS, |tx| {
        let mut counts = Counts::default();
        let mut dropped = Dropped::default();
        for entry in entries {
            let Body::Opened {
                hash,
                csp,
                plaintext,
                envelope,
            } = &entry.body
            else {
                continue;
            };
            // A REPLAY (a known nonce, or one written earlier in this zone): acked, nothing new.
            if store::contains_nonce(tx, NonceScope::D2d, hash)? {
                tracing::debug!("a reflected envelope came again; acked, nothing new");
                continue;
            }
            store::add_nonces(tx, NonceScope::D2d, std::slice::from_ref(hash))?;
            store::add_nonces(tx, NonceScope::Csp, csp)?;
            if let Some(error) = door_error(plaintext.len(), "defer an envelope failed") {
                return Err(error);
            }
            // C10 (TH6): a message this device took from the chat server as the leader, which the phone now reflects
            // as ITS leader's: the phone took it over, so the `pending` row of it goes (its nonce stays).
            if let Some(d2d::envelope::Content::IncomingMessage(m)) = &envelope.content {
                crate::leader::zone::forget_pending(tx, &m.sender_identity, m.message_id)?;
            }
            synced |= matches!(
                &envelope.content,
                Some(d2d::envelope::Content::ContactSync(_) | d2d::envelope::Content::GroupSync(_))
            );
            let mut notice_tag = [0; 8];
            notice_tag.copy_from_slice(&hash[..8]);
            let one = One {
                envelope,
                plaintext,
                server_at: entry.server_at,
                flags: entry.flags,
                notice_tag,
            };
            let events = map_one(
                tx,
                ctx,
                &one,
                DeferredSource::Reflected,
                &mut counts,
                &mut dropped,
            )?;
            // The envelope's events in order, durable before the ack: ONE row, or rows of at most 500
            // (S-L3: a settings delta over many stored contacts), all in this zone.
            outbox::append_in_rows(tx, &events, ctx.now_ms, ctx.wake)?;
        }
        store::warn_dropped(tx, dropped);
        Ok(counts)
    })?;
    ZONES.fetch_add(1, Ordering::SeqCst);
    if let (true, Some(pictures)) = (synced, ctx.pictures) {
        pictures.notify_one();
    }
    Ok(counts)
}

/// One opened `d2d.Envelope` for [`map_one`]: the envelope, its plaintext exactly as it came (what a `deferred` row
/// keeps: never a prost re-encode) and what the transport said with it.
pub(crate) struct One<'a> {
    pub(crate) envelope: &'a d2d::Envelope,
    pub(crate) plaintext: &'a [u8],
    /// `server_at` of its events and of a `deferred` row: the mediator's `Reflected.timestamp` for a reflection, the
    /// message's own time for a message the leader took from the chat server (TH6, critic T-L3).
    pub(crate) server_at: u64,
    /// `Reflected.flags`, or the CSP message's flags: a `deferred` row's `flags`.
    pub(crate) flags: u16,
    /// A notice's stable id (`map::Cx::notice_tag`).
    pub(crate) notice_tag: [u8; 8],
}

/// WHAT AN ENVELOPE IS, inside the caller's zone (TH6: factored out of the batch zone, so the follower's zone and the
/// leader's ZONE RECEIVE map through ONE mapper): [`classify`] and [`map`] over the file as this zone sees it, its BLOB
/// writes (a later envelope of the zone reads them), a `deferred` row of `source` with the plaintext as it came under
/// [`classify::MAP_VERSION`], and the rules it dropped, warned or skipped counted into `counts` (the owner writes the
/// lines once per zone) and the bounds' drops into `dropped` (likewise). Returns its EVENTS, in order, for the owner
/// to write into `core_outbox` in the same zone (`outbox::append_in_rows`). Nonces and acks are the owner's.
pub(crate) fn map_one(
    tx: &mut Tx<'_>,
    ctx: &Ctx<'_>,
    one: &One<'_>,
    source: DeferredSource,
    counts: &mut Counts,
    dropped: &mut Dropped,
) -> Result<Vec<BackendEvent>> {
    // Reads see this zone's earlier writes (one transaction).
    let cx = map::Cx {
        own: ctx.own.as_str(),
        now_ms: ctx.now_ms,
        server_at: one.server_at,
        catch_up: ctx.catch_up,
        notice_tag: one.notice_tag,
        remap: false,
    };
    let mapped = map::map(one.envelope, &cx, &StoreView(tx))?;
    if let Some(rule) = mapped.dropped {
        counts.add(rule);
    }
    if let Some(rule) = mapped.warned {
        counts.warn_applied(rule);
    }
    for &(rule, n) in &mapped.skipped {
        counts.skip(rule, n);
    }
    let created_ms = i64::try_from(one.server_at).unwrap_or(ctx.now_ms);
    apply_writes(tx, &mapped.writes, created_ms)?;
    if let Some(kind) = mapped.deferred {
        let row = NewDeferred {
            source,
            kind,
            map_version: classify::MAP_VERSION,
            server_at: one.server_at,
            flags: one.flags,
            envelope: one.plaintext,
        };
        let d = store::add_deferred(tx, &row, ctx.now_ms, &ctx.bounds)?;
        dropped.full += d.full;
        dropped.expired += d.expired;
    }
    Ok(mapped.events)
}

/// The BLOBs a mapped envelope writes ([`map::Write`]), in order, inside the caller's zone (the batch's,
/// or the re-map's: H5's bring-back of a re-mapped message is written in the zone of its events).
fn apply_writes(tx: &Connection, writes: &[map::Write], created_ms: i64) -> Result<()> {
    for write in writes {
        match write {
            map::Write::Contact(sync) => store::put_contact(tx, sync)?,
            map::Write::Group {
                creator,
                group_id,
                sync,
            } => store::put_group(tx, creator, *group_id, sync, created_ms)?,
            map::Write::GroupDeleted { creator, group_id } => {
                store::delete_group(tx, creator, *group_id)?;
            }
            map::Write::Settings(sync) => store::put_settings(tx, sync)?,
            map::Write::Profile(sync) => store::put_profile(tx, sync)?,
            map::Write::List(sync) => store::put_list(tx, sync, created_ms)?,
            map::Write::ListDeleted { id } => store::delete_list(tx, *id)?,
        }
    }
    Ok(())
}

/// The map's reads of the file, inside the zone's transaction.
struct StoreView<'a>(&'a Connection);

impl map::View for StoreView<'_> {
    fn contact(&self, identity: &str) -> Result<bool> {
        store::contact_exists(self.0, identity)
    }

    fn group(&self, creator: &str, group_id: u64) -> Result<bool> {
        store::group_exists(self.0, creator, group_id)
    }

    fn stored_contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>> {
        store::load_contact(self.0, identity)
    }

    fn stored_group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>> {
        store::load_group(self.0, creator, group_id)
    }

    fn blocked(&self, identity: &str) -> Result<bool> {
        store::is_blocked(self.0, identity)
    }

    fn sync_counts(&self) -> Result<(u64, u64)> {
        store::sync_counts(self.0)
    }

    fn stored_settings(&self) -> Result<Option<d2d_sync::Settings>> {
        store::load_settings(self.0)
    }

    fn stored_profile(&self) -> Result<Option<d2d_sync::UserProfile>> {
        store::load_profile(self.0)
    }

    fn stored_list(&self, id: u64) -> Result<Option<d2d_sync::DistributionList>> {
        store::load_list(self.0, id)
    }

    fn list_count(&self) -> Result<u64> {
        store::list_count(self.0)
    }
}

/// H3's probe: the D2D nonce of an envelope whose own zone failed, alone (one row, no kill point).
fn nonce_alone(db: &ZoneDb, hash: &[u8; 32]) -> Result<()> {
    db.zone(|tx| store::add_nonces(tx, NonceScope::D2d, std::slice::from_ref(hash)))
}

/// The dropped envelopes of one batch per rule, in the order first seen, the envelopes applied with a
/// counted WARN (H6 `contact-key-change`), and the parts of update envelopes skipped (R10, R11).
#[derive(Default)]
pub(crate) struct Counts(
    Vec<(&'static str, u32)>,
    Vec<(&'static str, u32)>,
    Vec<(&'static str, u32)>,
);

impl Counts {
    fn skip(&mut self, rule: &'static str, n: u32) {
        match self.2.iter_mut().find(|(r, _)| *r == rule) {
            Some((_, total)) => *total += n,
            None => self.2.push((rule, n)),
        }
    }

    fn warn_applied(&mut self, rule: &'static str) {
        match self.1.iter_mut().find(|(r, _)| *r == rule) {
            Some((_, total)) => *total += 1,
            None => self.1.push((rule, 1)),
        }
    }

    fn add(&mut self, rule: &'static str) {
        self.add_n(rule, 1);
    }

    fn add_n(&mut self, rule: &'static str, n: u32) {
        match self.0.iter_mut().find(|(r, _)| *r == rule) {
            Some((_, total)) => *total += n,
            None => self.0.push((rule, n)),
        }
    }

    fn merge(&mut self, other: Counts) {
        for (rule, n) in other.0 {
            self.add_n(rule, n);
        }
        for (rule, n) in other.1 {
            for _ in 0..n {
                self.warn_applied(rule);
            }
        }
        for (rule, n) in other.2 {
            self.skip(rule, n);
        }
    }

    /// TH6: the same lines for the messages the LEADER took from the chat server (ZONE RECEIVE): a message the map
    /// drops is still reflected and acked by the leader (the phone maps it as its own), so the words say "mapped to
    /// nothing", not "acked and dropped". Rule names and counts only.
    pub(crate) fn warn_received(self) {
        for (rule, n) in self.1 {
            tracing::warn!(
                rule,
                applied = n,
                "received syncs applied without a field: {n} by {rule}"
            );
        }
        for (rule, n) in self.2 {
            if classify::is_quiet(rule) {
                tracing::debug!(
                    rule,
                    skipped = n,
                    "received message updates skipped: {n} by {rule}"
                );
            } else {
                tracing::warn!(
                    rule,
                    skipped = n,
                    "received message updates skipped: {n} by {rule}"
                );
            }
        }
        for (rule, n) in self.0 {
            if classify::is_quiet(rule) {
                tracing::debug!(
                    rule,
                    dropped = n,
                    "received messages mapped to nothing: {n} by {rule}"
                );
            } else {
                tracing::warn!(
                    rule,
                    dropped = n,
                    "received messages mapped to nothing: {n} by {rule}"
                );
            }
        }
    }

    /// One line per rule (its name and count only): a WARN, except the rules that are the protocol's normal
    /// path ([`classify::is_quiet`]), which are DEBUG.
    fn warn(self) {
        for (rule, n) in self.1 {
            tracing::warn!(
                rule,
                applied = n,
                "reflected syncs applied without a field: {n} by {rule}"
            );
        }
        for (rule, n) in self.2 {
            // The protocol's normal paths (FIX-P, P-M1: the Sent update of a notes group) are DEBUG.
            if classify::is_quiet(rule) {
                tracing::debug!(
                    rule,
                    skipped = n,
                    "reflected message updates skipped: {n} by {rule}"
                );
            } else {
                tracing::warn!(
                    rule,
                    skipped = n,
                    "reflected message updates skipped: {n} by {rule}"
                );
            }
        }
        for (rule, n) in self.0 {
            if classify::is_quiet(rule) {
                tracing::debug!(
                    rule,
                    dropped = n,
                    "reflected envelopes acked and dropped: {n} by {rule}"
                );
            } else {
                tracing::warn!(
                    rule,
                    dropped = n,
                    "reflected envelopes acked and dropped: {n} by {rule}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(own: &str) -> Keys {
        Keys {
            dgk: DeviceGroupKey::random(),
            own: ThreemaId::try_from(own).expect("a synthetic ID"),
        }
    }

    fn hex(bytes: [u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The nonce stores' form, as literals computed outside the crate (HMAC-SHA256, key = the eight bytes of the own
    /// ID, message = the 24 nonce bytes): the same nonce hashes differently under another ID, and `Keys::hash` is
    /// the one call both the loop and the send core make.
    #[test]
    fn a_nonce_is_hashed_with_the_own_id() {
        let nonce = Nonce([5; Nonce::LENGTH]);
        assert_eq!(
            hex(keys("A6OWN001").hash(&nonce)),
            "283f31e88456ac9667d1a0a2e061070f581f09e9cf4d592b52c858b1a8e64e0a"
        );
        assert_eq!(
            hex(keys("A6OTH002").hash(&nonce)),
            "608c20ea2c34414b13fc72dc97f158556d3cafaede71e3c5895895d3a52ec517"
        );
    }
}
