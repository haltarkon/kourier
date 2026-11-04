//! Safety numbers (P5a, architecture §6.9): the number a person compares with their phone.
//!
//! Signal's form today (Desktop `ts/util/safetyNumber.preload.ts`, Android
//! `VerifyDisplayScreenViewModel.kt`): libsignal's `Fingerprint` of version 2 with 5,200
//! iterations, each side's id the 16 raw bytes of its ACI (never the 17-byte service-id
//! binary, never the string), OUR side our ACI identity key (never the PNI one), THEIR side the
//! key stored for their ACI. No ACI, no number (Desktop `isSafetyNumberNotAvailable.std.ts`).
//! Key Transparency (Desktop 8.32) is out of scope.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, BackendNotice, IncomingMessage};
use kourier_db::Tx;
use kourier_db::rusqlite::Connection;
use kourier_model::{
    Body, ContactPatch, ConvKey, IdentityFingerprint, Origin, Patch, Redacted, SafetyAnswer,
    SafetyNumber, SafetyUnavailable, SystemEvent, UserId, VerifyState,
};
use libsignal_core::{Aci, ServiceId};
use libsignal_protocol::{Fingerprint, FingerprintError, IdentityKey};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::jobs::{self, Done, Job, JobKind};
use crate::net::SignalTransport;
use crate::send::{self, Delivery, Outgoing};
use crate::service_id::{SignalUser, parse_user_id};
use crate::storage::intents::IdentityFiled;
use crate::store::{IDENTITY_DEFAULT, IDENTITY_UNVERIFIED, IDENTITY_VERIFIED};

/// The fingerprint version Signal shows and scans (`SERVICE_ID_VERSION`).
const VERSION: u32 = 2;
/// The hash iterations of each side.
const ITERATIONS: u32 = 5200;

/// libsignal's fingerprint with Signal's version and iterations.
fn fingerprint(
    local_id: &[u8],
    local_key: &IdentityKey,
    remote_id: &[u8],
    remote_key: &IdentityKey,
) -> std::result::Result<Fingerprint, FingerprintError> {
    Fingerprint::new(
        VERSION, ITERATIONS, local_id, local_key, remote_id, remote_key,
    )
}

/// The 60 digits and OUR side's scannable bytes for one pair of ACIs and keys.
pub(crate) fn compute(
    our_aci: Aci,
    our_key: &IdentityKey,
    their_aci: Aci,
    their_key: &IdentityKey,
) -> Result<(String, Vec<u8>)> {
    let ours = Uuid::from(our_aci);
    let theirs = Uuid::from(their_aci);
    let fp = fingerprint(ours.as_bytes(), our_key, theirs.as_bytes(), their_key)
        .map_err(|_| Error::Internal("safety number"))?;
    let digits = fp
        .display_string()
        .map_err(|_| Error::Internal("safety number digits"))?;
    let scannable = fp
        .scannable
        .serialize()
        .map_err(|_| Error::Internal("safety number scannable"))?;
    Ok((digits, scannable))
}

/// What the store holds for a safety number: their ACI, their stored key and its state, or
/// why there is none. Read in a zone; [`answer`] computes outside it.
#[derive(Clone)]
pub(crate) enum Lookup {
    /// A key is stored for the person's ACI.
    Found {
        /// Their ACI.
        aci: Aci,
        /// Their stored identity key.
        key: IdentityKey,
        /// `identities.verified` of it.
        verified: i64,
    },
    /// No number.
    Unavailable(SafetyUnavailable),
}

/// Reads what [`answer`] needs for `user`.
pub(crate) fn lookup(conn: &Connection, account: &Account, user: &UserId) -> Result<Lookup> {
    let aci = match parse_user_id(user) {
        Ok(SignalUser::Aci(aci)) => aci,
        // A PNI-only or number-only person (or an id that is no Signal id): no ACI.
        _ => return Ok(Lookup::Unavailable(SafetyUnavailable::NoAccountId)),
    };
    if aci == account.aci {
        return Ok(Lookup::Unavailable(SafetyUnavailable::OwnId));
    }
    let sid = libsignal_core::ServiceId::from(aci).service_id_string();
    Ok(match crate::store::load_identity(conn, &sid)? {
        None => Lookup::Unavailable(SafetyUnavailable::NoKey),
        Some((key, verified)) => Lookup::Found { aci, key, verified },
    })
}

/// The answer to `Command::SafetyNumber` from a [`Lookup`].
pub(crate) fn answer(account: &Account, found: Lookup) -> Result<SafetyAnswer> {
    let (aci, key, verified) = match found {
        Lookup::Unavailable(why) => return Ok(Err(why)),
        Lookup::Found { aci, key, verified } => (aci, key, verified),
    };
    let state = match verified {
        crate::store::IDENTITY_DEFAULT => VerifyState::Default,
        crate::store::IDENTITY_VERIFIED => VerifyState::Verified,
        crate::store::IDENTITY_UNVERIFIED => VerifyState::Changed,
        _ => return Err(Error::Decode("identity verified state")),
    };
    let (digits, scannable) = compute(account.aci, account.aci_identity.identity_key(), aci, &key)?;
    Ok(Ok(SafetyNumber {
        digits: Redacted::new(digits),
        scannable: Redacted::new(scannable),
        key: IdentityFingerprint(key.serialize().to_vec()),
        state,
    }))
}

/// How many people's answers [`Memo`] keeps; past it the memo starts again (the level asks
/// about one person at a time, so a few entries serve every re-ask).
const MEMO_PEOPLE: usize = 32;

/// The last answer per person (P5 critic F16): every contact change of the account re-asks an
/// open safety-number level, and a number is 10,400 SHA-512. An entry is used only while OUR
/// key, THEIR stored key and the stored state are all the ones it was computed from, so a key
/// change (theirs or ours) or a Mark is a miss; a hit costs the one indexed read of
/// [`lookup`]. In memory only, on the actor thread.
#[derive(Default)]
pub(crate) struct Memo {
    entries: RefCell<HashMap<Aci, Remembered>>,
}

/// One remembered answer and what it was computed from.
struct Remembered {
    ours: IdentityKey,
    theirs: IdentityKey,
    verified: i64,
    number: SafetyNumber,
}

impl Memo {
    /// The remembered answer for `found`, when it was computed from the same keys and state.
    pub(crate) fn hit(&self, account: &Account, found: &Lookup) -> Option<SafetyNumber> {
        let Lookup::Found { aci, key, verified } = found else {
            return None;
        };
        let entries = self.entries.borrow();
        let entry = entries.get(aci)?;
        (entry.ours == *account.aci_identity.identity_key()
            && entry.theirs == *key
            && entry.verified == *verified)
            .then(|| entry.number.clone())
    }

    /// Remembers the answer computed for `found`.
    pub(crate) fn remember(&self, account: &Account, found: &Lookup, number: &SafetyNumber) {
        let Lookup::Found { aci, key, verified } = found else {
            return;
        };
        let mut entries = self.entries.borrow_mut();
        if entries.len() >= MEMO_PEOPLE && !entries.contains_key(aci) {
            entries.clear();
        }
        entries.insert(
            *aci,
            Remembered {
                ours: *account.aci_identity.identity_key(),
                theirs: *key,
                verified: *verified,
                number: number.clone(),
            },
        );
    }
}

/// How long a [`JobKind::VerifiedSync`] is retried: the phone hears a state at most a day late.
pub(crate) const VERIFIED_SYNC_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

/// The payload of a [`JobKind::VerifiedSync`]: the state the user set, the key it was set
/// for (the key the page showed, call 5) and when he clicked.
#[derive(Serialize, Deserialize)]
struct SyncPayload {
    /// `identities.verified` as set: 0 or 1.
    state: i64,
    /// Their identity key, serialized with its type byte (33 bytes).
    key: Vec<u8>,
    /// The time of the click that set `state`, ms (the lead's R3: the latest click wins by
    /// time, see [`apply_remote_state`]).
    #[serde(default)]
    set_at: i64,
}

/// What [`set_verified`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SetOutcome {
    /// Stored, told to the core and on its way to our other devices (a `VerifiedSync` job, or
    /// an `identity` intent of the Storage Service write when the account writes its chat
    /// settings and the phone's record of the person holds the same key; for a record that holds
    /// another key or none the intent is filed AND the job is queued, C5b).
    Done,
    /// Refused; nothing changed. A log-safe reason.
    Refused(&'static str),
}

/// Mark as Verified (`verified`) and Clear Verification (`!verified`: Desktop's
/// `setVerified`, SD `ts/models/conversations.preload.ts`) of `user`'s key `key`; Confirm New
/// Safety Number reaches this only for a person in state 2
/// ([`crate::commands::confirm_identity`], BQ-2b), in ONE zone: the stored key must be `key` (else the
/// number changed under the page and the mark is refused); the state becomes 1 or 0; the core
/// gets the contact's trust and key, the quiet notice in the 1:1 chat when the state changed
/// (only a chat the core has: a notice opens no chat; P5 critic F10: authored by the contact,
/// silent, the wording from the `SystemEvent`), `IdentityTrusted` when the state was 2 (the
/// paused sends go on); and ONE `VerifiedSync` job per person carries the newest state to our
/// other devices (its payload replaced, re-run when asked during a run: critic H3). With the
/// account's switch on and a contact record of the person on the phone that holds the SAME key, the
/// state travels as an `identity` intent instead and no job is queued; when that record holds
/// another key or none the intent is filed and the job is queued too (A10c, see the end of the
/// function).
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_verified(
    tx: &mut Tx<'_>,
    account: &Account,
    user: &UserId,
    verified: bool,
    key: &IdentityFingerprint,
    now: i64,
    stamp: u64,
    wake: &Rc<Notify>,
) -> Result<SetOutcome> {
    let aci = match parse_user_id(user) {
        Ok(SignalUser::Aci(aci)) => aci,
        _ => return Ok(SetOutcome::Refused("this person has no account id")),
    };
    if aci == account.aci {
        return Ok(SetOutcome::Refused(
            "there is no safety number with ourselves",
        ));
    }
    let sid = ServiceId::from(aci).service_id_string();
    let Some((stored, was)) = crate::store::load_identity(tx, &sid)? else {
        return Ok(SetOutcome::Refused("no identity key is stored"));
    };
    if stored.serialize()[..] != key.0[..] {
        return Ok(SetOutcome::Refused("the safety number changed"));
    }
    let state = if verified {
        IDENTITY_VERIFIED
    } else {
        IDENTITY_DEFAULT
    };
    if was != state {
        tx.prepare_cached(
            "UPDATE identities SET verified = ?2, updated_at = ?3 WHERE service_id = ?1",
        )?
        .execute((&sid, state, now))?;
    }
    let mut patch = ContactPatch::new(user.clone());
    patch.trust = Some(crate::store::trust_of(state));
    patch.identity_fingerprint = Patch::Set(key.clone());
    let mut events = vec![BackendEvent::ContactUpsert(patch)];
    if was != state && crate::peers::load(tx, &sid)?.features_hash.is_some() {
        events.push(BackendEvent::Message(notice(
            user, key, verified, now, stamp,
        )));
    }
    if was == IDENTITY_UNVERIFIED {
        events.push(BackendEvent::Notice(BackendNotice::IdentityTrusted {
            user: user.clone(),
        }));
    }
    crate::outbox::append_events(tx, &events, now, wake)?;
    // The stored key is trusted now (state 0 or 1): the jobs that wait after an untrusted
    // attempt are due (P5 M1); the actor wakes the runner after this zone.
    jobs::release_untrusted(tx, now)?;
    // An account that writes its chat settings to the Storage Service (the switch is on, A10c)
    // tells its other devices through the contact record: the state is an intent
    // `(contact:<aci>, identity)`, which the write carries as `identityState` beside the key
    // (the record Desktop's `captureChange` writes). When the phone's record holds the SAME key
    // NO sync message is queued: a second route would tell the phone twice, and while this
    // device's own job is queued the merge of the phone's record is `Remote::Kept`, so a newer
    // state of the phone could never win (A10b's open item 2). The intent takes part in "the
    // latest click wins by time" itself (V1: [`apply_remote_state`] reads its `created_at` for a
    // Verified sync, and the write job waits for `Online`). A job queued by an earlier click,
    // while the switch was off, carries an OLDER state than the intent: it goes. When the
    // phone's record holds ANOTHER key or none (C5b), the write cannot carry the state (the plan
    // states `identityState` only onto a record of the same key, `Why::OtherKey`) and drops the
    // intent a second later: the intent stays filed (A10b's rule and base are unchanged) AND the
    // sync is queued below, as with the switch off; both routes carry the same state for the same
    // key, so the phone hears it at least once (`Remote::Kept` lasts until our job has run). With
    // the switch off, or for a person the phone has no record of, no intent is filed and the sync
    // message is the only way, exactly as before.
    let filed =
        crate::storage::intents::intend_identity(tx, account, &sid, state, was, &key.0, now)?;
    if filed == IdentityFiled::SameKey {
        tx.prepare_cached("DELETE FROM jobs WHERE kind = ?1 AND key = ?2")?
            .execute((JobKind::VerifiedSync.as_str(), &sid))?;
        return Ok(SetOutcome::Done);
    }
    queue_verified_sync(tx, &sid, state, &key.0, now, now)?;
    Ok(SetOutcome::Done)
}

/// Queues the person's [`JobKind::VerifiedSync`] (one job per person, a later payload replaces an
/// earlier one): `state` as set for `key` at `set_at` (the click's time, which the latest-click
/// rule reads), expiring [`VERIFIED_SYNC_MAX_AGE`] after `now`. The runner sends it only while the
/// key and the state still equal the stored row (`verified_sync_content`). Called by
/// [`set_verified`] and, since the A10c fix round (V2), by `storage::intents::drop_where` for an
/// `identity` intent that is dropped: neither the write nor the sync would tell the other devices
/// of that click.
pub(crate) fn queue_verified_sync(
    conn: &Connection,
    sid: &str,
    state: i64,
    key: &[u8],
    set_at: i64,
    now: i64,
) -> Result<()> {
    let payload = serde_json::to_vec(&SyncPayload {
        state,
        key: key.to_vec(),
        set_at,
    })
    .map_err(|_| Error::Internal("verified sync payload"))?;
    jobs::enqueue_payload(
        conn,
        JobKind::VerifiedSync,
        sid,
        &payload,
        now,
        Some(VERIFIED_SYNC_MAX_AGE),
    )
}

/// The quiet notice of a state this device set: authored by the contact (as the key-change
/// notice, `stage1::identity_events`), silent (no unread, no notification), its id
/// `verified:<first 8 bytes of SHA-256(key), hex>:<unique timestamp>`: one per click, the same
/// when the outbox row is delivered twice.
fn notice(
    user: &UserId,
    key: &IdentityFingerprint,
    verified: bool,
    now: i64,
    stamp: u64,
) -> IncomingMessage {
    let local = format!("verified:{}:{stamp}", key_digest(key));
    let mut message = IncomingMessage::new(
        ConvKey::Direct(user.clone()),
        local,
        Origin::Remote {
            author: user.clone(),
            device: None,
        },
        Body::System(SystemEvent::VerifiedChanged {
            user: user.clone(),
            verified,
            other_device: false,
        }),
        now,
        now,
    );
    message.silent = true;
    message
}

/// Where a verified state set on another of our devices comes from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RemoteSource {
    /// A `SyncMessage.verified` from our own account, sent at `timestamp` (the envelope's).
    Sync {
        /// The envelope timestamp: the notice's id (`verified:<timestamp>`).
        timestamp: u64,
    },
    /// A Storage Service contact record (its notice id
    /// `verified:<hex8(sha256(key))>:<state>:<merge time in ms>`: fixed for one outbox row, so
    /// the core's ingest stays idempotent, and unique per applied change, so a phone that
    /// toggles back to a state it had before still leaves a notice, critic M2.3, the lead's
    /// R10).
    Storage,
}

/// What [`apply_remote_state`] did.
#[derive(Debug)]
pub(crate) enum Remote {
    /// The message names another key than the stored one: NOTHING was written (a message never
    /// replaces a key, BQ-1 (a)); the caller queues a profile fetch, which is authoritative.
    OtherKey,
    /// A Storage record while a [`JobKind::VerifiedSync`] of this device for the same key is still
    /// queued, or a Verified sync that is not newer than the click (the queued job's `set_at`, or
    /// the live identity intent's `created_at`, V1): the message is older than the user's own
    /// click here, which is on its way to the phone; nothing written (critic M2.4, the lead's R3).
    Kept,
    /// Applied (the state written, or the key inserted with it when there was no row): the
    /// contact's trust and key for the core, and the notice / release events to append after
    /// the contact's upsert.
    Applied {
        /// The trust of the state now stored.
        trust: kourier_model::TrustLevel,
        /// The stored key (the message's, equal to the stored one or newly inserted).
        fingerprint: IdentityFingerprint,
        /// The quiet notice (state changed, the core has the 1:1 chat) and `IdentityTrusted`
        /// (the row left state 2).
        events: Vec<BackendEvent>,
        /// The row left state 2: the caller wakes the job runner after the zone (P5 M1).
        released: bool,
    },
}

/// A verified state set on another of our devices (BQ-1 (a) with critic F2/F4 and Appendix
/// M2), for `user` (ACI service id string `sid`) and the key `key` the message names; `state`
/// is `identities.verified` (0, 1, or 2 from a Storage record). No row -> the key is INSERTED
/// with the state (a key from our own account beats trust on first use, M2.1); the stored key
/// -> the state is written; ANOTHER key -> nothing is stored ([`Remote::OtherKey`]). A row
/// that leaves state 2 is released ([`BackendNotice::IdentityTrusted`] and
/// [`jobs::release_untrusted`]: the core resumes the paused sends, the waiting jobs are due,
/// H1/M2.2); a changed state of an existing row gets the quiet "... from another device" notice
/// in the 1:1 chat the core has (F10: authored by the contact, silent). While this device's own
/// Verified sync for the same key is queued the LATEST CLICK WINS BY TIME (R3): a Storage record
/// (no time) and a sync not newer than the click are [`Remote::Kept`]; a newer sync is applied
/// and the queued job is deleted. With the account's switch on the click may travel as a live
/// `identity` intent instead of a job (A10c): for a SYNC that intent is the click at its
/// `created_at` (V1: not newer -> [`Remote::Kept`], the intent stays; newer -> applied and the
/// intent retired, `intents::retire_identity`); a Storage record meets the intent in the
/// three-way rule (`merge_identity`). Our own id is the caller's to refuse.
pub(crate) fn apply_remote_state(
    conn: &Connection,
    user: &UserId,
    sid: &str,
    key: &IdentityKey,
    state: i64,
    now: i64,
    source: RemoteSource,
) -> Result<Remote> {
    let stored = crate::store::load_identity(conn, sid)?;
    let was = match &stored {
        Some((stored, _)) if stored != key => return Ok(Remote::OtherKey),
        Some((_, was)) => Some(*was),
        None => None,
    };
    let job_at = own_sync_set_at(conn, sid, key)?;
    match source {
        // A record carries no time and is older than the click that is still on its way to the
        // phone (critic M2.4). Only the queued job counts here: a live identity intent meets a
        // record in the three-way rule (`merge_identity`, `settle_identity`).
        RemoteSource::Storage if job_at.is_some() => return Ok(Remote::Kept),
        RemoteSource::Storage => {}
        // The latest click wins by time (the lead's R3, storage LOW-3, protocol MEDIUM-1): a
        // phone click that is not newer than ours is ignored (our job, or our intent, makes the
        // phone converge); a newer one is applied and our queued job for this person goes (it
        // would tell the phone our older state), and so does our live identity intent (V1: with the
        // account's switch on the click travels as that intent and no job exists; it is a click
        // at its `created_at`, exactly as the job's `set_at` was). A tie keeps ours.
        RemoteSource::Sync { timestamp } => {
            let intent_at =
                crate::storage::intents::identity_click_at(conn, sid, &key.serialize())?;
            if let Some(set_at) = job_at.into_iter().chain(intent_at).max() {
                if i64::try_from(timestamp).unwrap_or(i64::MAX) <= set_at {
                    return Ok(Remote::Kept);
                }
                if job_at.is_some() {
                    conn.prepare_cached("DELETE FROM jobs WHERE kind = ?1 AND key = ?2")?
                        .execute((JobKind::VerifiedSync.as_str(), sid))?;
                }
                if intent_at.is_some() {
                    crate::storage::intents::retire_identity(
                        conn,
                        sid,
                        &key.serialize(),
                        state,
                        i64::try_from(timestamp).unwrap_or(i64::MAX),
                        now,
                    )?;
                }
            }
        }
    }
    let fingerprint = IdentityFingerprint(key.serialize().into_vec());
    match was {
        None => {
            conn.prepare_cached(
                "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4)",
            )?
            .execute((sid, &fingerprint.0[..], state, now))?;
        }
        Some(was) if was != state => {
            conn.prepare_cached(
                "UPDATE identities SET verified = ?2, updated_at = ?3 WHERE service_id = ?1",
            )?
            .execute((sid, state, now))?;
        }
        Some(_) => {}
    }
    let mut events = Vec::new();
    let changed = was.is_some_and(|was| was != state);
    // `VerifiedChanged` says verified or not; a move INTO state 2 (a Storage record only) has
    // no wording of its own and gets no notice.
    if changed
        && state != IDENTITY_UNVERIFIED
        && crate::peers::load(conn, sid)?.features_hash.is_some()
    {
        let local = match source {
            RemoteSource::Sync { timestamp } => format!("verified:{timestamp}"),
            RemoteSource::Storage => {
                format!("verified:{}:{state}:{now}", key_digest(&fingerprint))
            }
        };
        let mut message = IncomingMessage::new(
            ConvKey::Direct(user.clone()),
            local,
            Origin::Remote {
                author: user.clone(),
                device: None,
            },
            Body::System(SystemEvent::VerifiedChanged {
                user: user.clone(),
                verified: state == IDENTITY_VERIFIED,
                other_device: true,
            }),
            match source {
                RemoteSource::Sync { timestamp } => i64::try_from(timestamp).unwrap_or(now),
                RemoteSource::Storage => now,
            },
            now,
        );
        message.silent = true;
        events.push(BackendEvent::Message(message));
    }
    let released = was == Some(IDENTITY_UNVERIFIED) && state != IDENTITY_UNVERIFIED;
    if released {
        events.push(BackendEvent::Notice(BackendNotice::IdentityTrusted {
            user: user.clone(),
        }));
        jobs::release_untrusted(conn, now)?;
    }
    Ok(Remote::Applied {
        trust: crate::store::trust_of(state),
        fingerprint,
        events,
        released,
    })
}

/// The first 8 bytes of SHA-256 of a serialized key, in hex (notice ids; never logged).
fn key_digest(key: &IdentityFingerprint) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(&key.0);
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The time of the click (`set_at`, ms) that this device's own [`JobKind::VerifiedSync`] for
/// `sid` and the same key still carries, when that job is queued (one row per person: the
/// `(kind, key)` unique index).
fn own_sync_set_at(conn: &Connection, sid: &str, key: &IdentityKey) -> Result<Option<i64>> {
    use kourier_db::rusqlite::OptionalExtension as _;
    let payload: Option<Option<Vec<u8>>> = conn
        .prepare_cached("SELECT payload FROM jobs WHERE kind = ?1 AND key = ?2")?
        .query_row((JobKind::VerifiedSync.as_str(), sid), |r| r.get(0))
        .optional()?;
    Ok(payload
        .flatten()
        .and_then(|p| serde_json::from_slice::<SyncPayload>(&p).ok())
        .filter(|p| p.key[..] == key.serialize()[..])
        .map(|p| p.set_at))
}

/// The `SyncMessage.verified` a [`JobKind::VerifiedSync`] job sends, or `None` when it must be
/// dropped: its payload does not parse, the person has no stored key any more, the stored key
/// is no longer the one the user marked (critic M5: Android would SAVE the payload's old key
/// and archive its sessions, `IdentityUtil.java`), or the stored state is no longer the one he
/// set (the lead's R3).
pub(crate) fn verified_sync_content(
    conn: &Connection,
    job: &Job,
) -> Result<Option<crate::proto::Content>> {
    let Some(payload) = job
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_slice::<SyncPayload>(p).ok())
    else {
        tracing::warn!("verified sync job without a valid payload dropped");
        return Ok(None);
    };
    let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(&job.key) else {
        tracing::warn!("verified sync job without an ACI dropped");
        return Ok(None);
    };
    match crate::store::load_identity(conn, &job.key)? {
        // The key AND the state are the click's (the lead's R3, belt): a state that moved since
        // (another device's click applied, a key that left state 1) is not told to the phone.
        Some((stored, state))
            if stored.serialize()[..] == payload.key[..] && state == payload.state => {}
        _ => {
            tracing::info!(job = %job.log_key(), "verified sync dropped: key or state changed");
            return Ok(None);
        }
    }
    let state = match payload.state {
        IDENTITY_VERIFIED => crate::proto::verified::State::Verified,
        _ => crate::proto::verified::State::Default,
    };
    Ok(Some(crate::send::content::verified_sync(
        aci,
        &payload.key,
        state,
    )))
}

/// Runs a [`JobKind::VerifiedSync`]: the state to our own devices (Desktop's
/// `getVerificationSync`; sent whatever the sync settings say, his "Yes, always").
pub(crate) async fn run_verified_sync<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let Some(sync) = shared.db.read(|c| verified_sync_content(c, job))? else {
        return Ok(Done::Finished);
    };
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    send::send_to(
        &ctx,
        account.aci.into(),
        &Outgoing {
            content: &sync,
            timestamp: shared.unique_timestamp(),
            delivery: Delivery::TRANSCRIPT,
            local: None,
            group: None,
        },
    )
    .await?;
    tracing::debug!(job = %job.log_key(), "verified sync sent");
    Ok(Done::Finished)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use libsignal_protocol::{IdentityKeyPair, PrivateKey};

    use super::*;

    // libsignal v0.104.0 `rust/protocol/src/fingerprint.rs` `ALICE_IDENTITY` / `BOB_IDENTITY`.
    const ALICE_IDENTITY: &str =
        "0506863bc66d02b40d27b8d49ca7c09e9239236f9d7d25d6fcca5ce13c7064d868";
    const BOB_IDENTITY: &str = "05f781b6fb32fed9ba1cf2de978d4d5da28dc34046ae814402b5c0dbd96fda907b";

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn tohex(b: &[u8]) -> String {
        b.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn key(hex: &str) -> IdentityKey {
        IdentityKey::decode(&unhex(hex)).expect("key")
    }

    fn aci(s: &str) -> Aci {
        Aci::from(Uuid::parse_str(s).expect("uuid"))
    }

    fn user(a: Aci) -> UserId {
        crate::service_id::aci_user_id(a).expect("id")
    }

    /// A backend.db with `key` stored for `who` in state `verified`.
    fn db_with(
        who: Aci,
        key: &IdentityKey,
        verified: i64,
    ) -> (tempfile::TempDir, kourier_db::ZoneDb) {
        let dir = tempfile::tempdir().expect("dir");
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([3; 32]),
        )
        .expect("open");
        let sid = libsignal_core::ServiceId::from(who).service_id_string();
        db.zone(|tx| -> Result<usize> {
            Ok(tx.execute(
                "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
                 VALUES (?1, ?2, ?3, 0, 1)",
                (sid, &key.serialize()[..], verified),
            )?)
        })
        .expect("insert");
        (dir, db)
    }

    fn ask(db: &kourier_db::ZoneDb, acct: &Account, who: &UserId) -> SafetyAnswer {
        let found = db.read(|c| lookup(c, acct, who)).expect("lookup");
        answer(acct, found).expect("answer")
    }

    /// KAT 1: libsignal's own `fingerprint_test_v2` (ids are phone numbers there): our version
    /// and iterations give its digits and Alice's version-2 scannable.
    #[test]
    fn libsignal_v2_vector() {
        let fp = fingerprint(
            b"+14152222222",
            &key(ALICE_IDENTITY),
            b"+14153333333",
            &key(BOB_IDENTITY),
        )
        .expect("fingerprint");
        assert!(
            fp.display_string().expect("digits")
                == "300354477692869396892869876765458257569162576843440918079131",
            "the digits are not libsignal's version-2 test vector"
        );
        assert!(
            tohex(&fp.scannable.serialize().expect("scannable"))
                == "080212220a201e301a0353dce3dbe7684cb8336e85136cdc0ee96219494ada305d62a7bd61df1a220a20d62cbf73a11592015b6b9f1682ac306fea3aaf3885b84d12bca631e9d4fb3a4d",
            "the scannable is not libsignal's ALICE_SCANNABLE_FINGERPRINT_V2"
        );
    }

    /// KAT 2: the ACI form Kourier uses (16 raw bytes per ACI). Computed independently by a
    /// stdlib Python oracle that first reproduced KAT 1 exactly (p5-refresh.md §1.3, re-run in
    /// P5a-C1):
    ///   h = sha512(b"\0\0" + key + id + key); 5199 times h = sha512(h + key)
    ///   side = "".join("%05d" % (int.from_bytes(h[o:o+5], "big") % 100000) for o in 0,5,..,25)
    ///   digits = the two sides sorted and joined
    ///   scannable = 08 02 12 22 0a 20 <local h[:32]> 1a 22 0a 20 <remote h[:32]>
    /// The same keys with the ids as 36-character strings give
    /// 776408462034749208497258649925924195369608150375109004140855 (the encoding mutant).
    #[test]
    fn the_aci_vector() {
        let (digits, scannable) = compute(
            aci("9d0652a3-dcc3-4d11-975f-74d61598733f"),
            &key(ALICE_IDENTITY),
            aci("796abedb-ca4e-4f18-8803-1fde5b921f9f"),
            &key(BOB_IDENTITY),
        )
        .expect("compute");
        assert!(
            digits == "446657246696264293037458627907774872764702919995219132117432",
            "the digits of the ACI vector differ"
        );
        assert!(
            tohex(&scannable)
                == "080212220a208a2c258a99951098bd3222aa59be88e53fad3f3712fbd9ed3aa9ba26092317791a220a200a6bb4b36fb0dc81a39f14f4acac47b1250386e1a5fccce93971d687a1b856b6",
            "the scannable of the ACI vector differs (our side must come first)"
        );
        assert!(scannable.len() == 74, "a version-2 scannable is 74 bytes");
    }

    /// The account's ACI and PNI keys differ: the number is made with the ACI one, and the
    /// answer names the stored key and its state. Also measures one computation (dev profile).
    #[test]
    fn our_pni_key_is_never_used() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_VERIFIED);
        let started = Instant::now();
        let got = ask(&db, &acct, &user(ada)).expect("a number");
        let took = started.elapsed();
        let (with_aci, _) =
            compute(acct.aci, acct.aci_identity.identity_key(), ada, &theirs).expect("aci");
        let (with_pni, _) =
            compute(acct.aci, acct.pni_identity.identity_key(), ada, &theirs).expect("pni");
        assert!(with_aci != with_pni, "the test needs two different keys");
        assert!(
            *got.digits.expose() == with_aci,
            "the number was not made with our ACI identity key"
        );
        assert!(
            got.key == IdentityFingerprint(theirs.serialize().to_vec()),
            "the answer must name the stored key"
        );
        assert!(got.state == VerifyState::Verified, "state 1 is Verified");
        eprintln!("one safety number took {} us", took.as_micros());
    }

    #[test]
    fn the_states_map() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        for (stored, want) in [
            (crate::store::IDENTITY_DEFAULT, VerifyState::Default),
            (crate::store::IDENTITY_VERIFIED, VerifyState::Verified),
            (crate::store::IDENTITY_UNVERIFIED, VerifyState::Changed),
        ] {
            let (_dir, db) = db_with(ada, &key(BOB_IDENTITY), stored);
            let got = ask(&db, &acct, &user(ada)).expect("a number");
            assert!(got.state == want, "stored state {stored} maps wrongly");
        }
    }

    #[test]
    fn no_key() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let other = aci("9d0652a3-dcc3-4d11-975f-74d61598733f");
        let (_dir, db) = db_with(other, &key(BOB_IDENTITY), 0);
        assert!(
            ask(&db, &acct, &user(ada)) == Err(SafetyUnavailable::NoKey),
            "a person without a stored key has no number yet"
        );
    }

    #[test]
    fn pni_only() {
        let acct = crate::db::test_account();
        let pni = libsignal_core::Pni::from(Uuid::new_v4());
        // A key stored for the PNI does not make a number.
        let dir = tempfile::tempdir().expect("dir");
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([3; 32]),
        )
        .expect("open");
        let pni_user = crate::service_id::pni_user_id(pni).expect("id");
        assert!(
            ask(&db, &acct, &pni_user) == Err(SafetyUnavailable::NoAccountId),
            "a PNI-only person has no number"
        );
        let e164 = crate::service_id::e164_user_id("+15550100777").expect("id");
        assert!(
            ask(&db, &acct, &e164) == Err(SafetyUnavailable::NoAccountId),
            "a number-only person has no number"
        );
    }

    #[test]
    fn own_id() {
        let acct = crate::db::test_account();
        let own_key = *acct.aci_identity.identity_key();
        let (_dir, db) = db_with(acct.aci, &own_key, 1);
        assert!(
            ask(&db, &acct, &user(acct.aci)) == Err(SafetyUnavailable::OwnId),
            "there is no safety number with ourselves"
        );
    }

    /// A key pair whose public half is libsignal's `ALICE_IDENTITY` (the pair is not checked).
    #[test]
    fn a_number_from_an_account_with_a_known_key() {
        let mut acct = crate::db::test_account();
        let private = PrivateKey::deserialize(&[7; 32]).expect("private");
        acct.aci_identity = IdentityKeyPair::new(key(ALICE_IDENTITY), private);
        acct.aci = aci("9d0652a3-dcc3-4d11-975f-74d61598733f");
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let (_dir, db) = db_with(ada, &key(BOB_IDENTITY), 0);
        let got = ask(&db, &acct, &user(ada)).expect("a number");
        assert!(
            *got.digits.expose() == "446657246696264293037458627907774872764702919995219132117432",
            "the answer's digits are not the ACI vector's"
        );
        assert!(
            format!("{got:?}").find("44665").is_none(),
            "Debug of a safety number must not print its digits"
        );
    }

    // ---- P5a-C4: Mark / Clear / Confirm, the Verified sync, the memo ----

    /// The events the zones wrote for the core, in order.
    fn core_events(db: &kourier_db::ZoneDb) -> Vec<BackendEvent> {
        db.read(|c| -> Result<Vec<Vec<u8>>> {
            Ok(c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?
                .query_map([], |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?)
        })
        .expect("outbox")
        .iter()
        .flat_map(|row| kourier_backend_api::decode_outbox_row(row).expect("row"))
        .collect()
    }

    /// The queued `VerifiedSync` jobs as the runner gets them.
    fn sync_jobs(db: &kourier_db::ZoneDb) -> Vec<Job> {
        db.read(|c| -> Result<Vec<Job>> {
            Ok(c.prepare(
                "SELECT id, key, payload, requests FROM jobs WHERE kind = 'verified_sync'",
            )?
            .query_map([], |r| {
                Ok(Job {
                    id: r.get(0)?,
                    kind: JobKind::VerifiedSync,
                    key: r.get(1)?,
                    attempts: 0,
                    expires_at: None,
                    payload: r.get(2)?,
                    requests: r.get(3)?,
                })
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?)
        })
        .expect("jobs")
    }

    fn stored_state(db: &kourier_db::ZoneDb, who: Aci) -> i64 {
        let sid = ServiceId::from(who).service_id_string();
        db.read(|c| -> Result<i64> {
            Ok(c.query_row(
                "SELECT verified FROM identities WHERE service_id = ?1",
                [sid],
                |r| r.get(0),
            )?)
        })
        .expect("state")
    }

    fn fp(key: &IdentityKey) -> IdentityFingerprint {
        IdentityFingerprint(key.serialize().to_vec())
    }

    fn set(
        db: &kourier_db::ZoneDb,
        acct: &Account,
        who: &UserId,
        verified: bool,
        key: &IdentityFingerprint,
        stamp: u64,
    ) -> SetOutcome {
        let wake = Rc::new(Notify::new());
        db.zone(|tx| set_verified(tx, acct, who, verified, key, 100, stamp, &wake))
            .expect("zone")
    }

    /// The core has the 1:1 chat with `who` (its features were stated).
    fn has_chat(db: &kourier_db::ZoneDb, who: Aci) {
        let sid = ServiceId::from(who).service_id_string();
        db.zone(|tx| crate::peers::set_features_hash(tx, &sid, 7, 1))
            .expect("features");
    }

    /// Call 5: a mark names the key the page showed; another stored key refuses it and
    /// changes nothing (no state, no event, no job).
    #[test]
    fn set_verified_refuses_another_key() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let (_dir, db) = db_with(ada, &key(BOB_IDENTITY), crate::store::IDENTITY_DEFAULT);
        let shown = fp(&key(ALICE_IDENTITY));
        assert!(
            set(&db, &acct, &user(ada), true, &shown, 1)
                == SetOutcome::Refused("the safety number changed"),
            "a mark of a key that is not the stored one must be refused"
        );
        assert!(
            stored_state(&db, ada) == crate::store::IDENTITY_DEFAULT,
            "a refused mark changed the state"
        );
        assert!(core_events(&db).is_empty(), "a refused mark told the core");
        assert!(sync_jobs(&db).is_empty(), "a refused mark queued a sync");
        // Ourselves, a PNI-only person, a person without a key: refused too.
        let own = fp(acct.aci_identity.identity_key());
        assert!(matches!(
            set(&db, &acct, &user(acct.aci), true, &own, 2),
            SetOutcome::Refused(_)
        ));
        let nobody = aci("9d0652a3-dcc3-4d11-975f-74d61598733f");
        assert!(matches!(
            set(&db, &acct, &user(nobody), true, &shown, 3),
            SetOutcome::Refused(_)
        ));
    }

    /// Mark then Clear: the state follows the last click, the core gets the trust each time,
    /// a notice per changed state, and ONE job per person whose payload is the last state
    /// (critic H3). A Clear of a Default key changes nothing and writes no notice.
    #[test]
    fn set_verified_writes_state_and_job_once() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_DEFAULT);
        has_chat(&db, ada);
        let shown = fp(&theirs);
        assert!(set(&db, &acct, &user(ada), true, &shown, 11) == SetOutcome::Done);
        assert!(stored_state(&db, ada) == crate::store::IDENTITY_VERIFIED);
        assert!(set(&db, &acct, &user(ada), false, &shown, 12) == SetOutcome::Done);
        assert!(stored_state(&db, ada) == crate::store::IDENTITY_DEFAULT);
        assert!(set(&db, &acct, &user(ada), false, &shown, 13) == SetOutcome::Done);
        let jobs = sync_jobs(&db);
        assert!(
            jobs.len() == 1,
            "one Verified sync job per person, not one per click"
        );
        let sid = ServiceId::from(ada).service_id_string();
        assert!(jobs[0].key == sid, "the job is keyed by their ACI");
        let payload: SyncPayload =
            serde_json::from_slice(jobs[0].payload.as_deref().expect("payload")).expect("json");
        assert!(
            payload.state == crate::store::IDENTITY_DEFAULT && payload.key == shown.0,
            "the job carries the last click's state and the shown key"
        );
        let events = core_events(&db);
        let trusts: Vec<kourier_model::TrustLevel> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ContactUpsert(p) => p.trust,
                _ => None,
            })
            .collect();
        assert!(
            trusts
                == [
                    kourier_model::TrustLevel::Verified,
                    kourier_model::TrustLevel::Unverified,
                    kourier_model::TrustLevel::Unverified
                ],
            "the core's trust follows each click"
        );
        let notices: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) => match &m.body {
                    Body::System(SystemEvent::VerifiedChanged { verified, .. }) => Some(*verified),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            notices == [true, false],
            "one notice per changed state, none for a click that changed nothing"
        );
        assert!(
            !events.iter().any(|e| matches!(e, BackendEvent::Notice(_))),
            "nothing was in state 2: no release notice"
        );
    }

    /// Critic F10: the notice is authored by the contact, silent (no unread, no
    /// notification), says "this device", and is only written into a chat the core has.
    #[test]
    fn the_notice_is_silent_and_authored_by_the_contact() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_DEFAULT);
        // No chat yet: the mark writes no notice (a notice never opens a chat).
        assert!(set(&db, &acct, &user(ada), true, &fp(&theirs), 21) == SetOutcome::Done);
        assert!(
            !core_events(&db)
                .iter()
                .any(|e| matches!(e, BackendEvent::Message(_))),
            "a notice was written into a chat the core does not have"
        );
        has_chat(&db, ada);
        assert!(set(&db, &acct, &user(ada), false, &fp(&theirs), 22) == SetOutcome::Done);
        let messages: Vec<IncomingMessage> = core_events(&db)
            .into_iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) => Some(m),
                _ => None,
            })
            .collect();
        assert!(messages.len() == 1, "one notice for the Clear");
        let m = &messages[0];
        assert!(m.silent, "the notice must be silent");
        assert!(
            m.conv == ConvKey::Direct(user(ada)),
            "the notice goes into the 1:1 chat"
        );
        assert!(
            m.origin
                == Origin::Remote {
                    author: user(ada),
                    device: None
                },
            "the notice is authored by the contact"
        );
        assert!(
            m.body
                == Body::System(SystemEvent::VerifiedChanged {
                    user: user(ada),
                    verified: false,
                    other_device: false
                }),
            "the notice says what this device set"
        );
    }

    /// Confirm of a key that changed after it was verified (state 2): DEFAULT, the quiet
    /// notice, and `IdentityTrusted` so the core releases the paused sends.
    #[test]
    fn a_mark_out_of_state_two_releases() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_UNVERIFIED);
        has_chat(&db, ada);
        assert!(set(&db, &acct, &user(ada), false, &fp(&theirs), 31) == SetOutcome::Done);
        assert!(stored_state(&db, ada) == crate::store::IDENTITY_DEFAULT);
        let events = core_events(&db);
        assert!(
            events.iter().any(|e| matches!(
                e,
                BackendEvent::Notice(BackendNotice::IdentityTrusted { user: u }) if *u == user(ada)
            )),
            "leaving state 2 must tell the core to release"
        );
    }

    /// Critic M5: a sync whose key is no longer the stored one is dropped (Android would save
    /// the old key); the same key is sent.
    #[test]
    fn a_stale_verified_sync_is_dropped() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_DEFAULT);
        assert!(set(&db, &acct, &user(ada), true, &fp(&theirs), 41) == SetOutcome::Done);
        let job = sync_jobs(&db).remove(0);
        assert!(
            db.read(|c| verified_sync_content(c, &job))
                .expect("read")
                .is_some(),
            "a sync of the stored key is sent"
        );
        // Their key changes before the job runs.
        let sid = ServiceId::from(ada).service_id_string();
        db.zone(|tx| -> Result<usize> {
            Ok(tx.execute(
                "UPDATE identities SET identity_key = ?2 WHERE service_id = ?1",
                (&sid, &key(ALICE_IDENTITY).serialize()[..]),
            )?)
        })
        .expect("new key");
        assert!(
            db.read(|c| verified_sync_content(c, &job))
                .expect("read")
                .is_none(),
            "a sync of a key that is no longer stored must be dropped"
        );
    }

    /// What the phone gets: their ACI as 16 raw bytes, the shown key WITH its type byte (33
    /// bytes, 0x05 first), the state, a null message; padded.
    #[test]
    fn the_sync_carries_the_shown_key_with_its_type_byte() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_DEFAULT);
        assert!(set(&db, &acct, &user(ada), true, &fp(&theirs), 51) == SetOutcome::Done);
        let job = sync_jobs(&db).remove(0);
        let content = db
            .read(|c| verified_sync_content(c, &job))
            .expect("read")
            .expect("a sync");
        let Some(crate::proto::content::Content::SyncMessage(sync)) = content.content else {
            panic!("the job must send a sync message");
        };
        assert!(sync.padding.is_some(), "the sync message is padded");
        let Some(crate::proto::sync_message::Content::Verified(v)) = sync.content else {
            panic!("the sync must be a Verified");
        };
        let key_bytes = v.identity_key.expect("identityKey");
        assert!(
            key_bytes.len() == 33
                && key_bytes[0] == 0x05
                && key_bytes == theirs.serialize().to_vec(),
            "identityKey must be the shown key with its type byte"
        );
        assert!(
            v.destination_aci_binary.as_deref() == Some(&Uuid::from(ada).as_bytes()[..]),
            "destinationAciBinary must be the 16 raw bytes of their ACI"
        );
        assert!(v.destination_aci.is_none(), "binary only (L7)");
        assert!(
            v.state == Some(crate::proto::verified::State::Verified as i32),
            "the state is VERIFIED"
        );
        assert!(
            v.null_message.as_ref().is_some_and(|n| !n.is_empty()),
            "a null message pads the sync"
        );
        // The lead's R8: a Clear puts DEFAULT on the wire (a sync that said VERIFIED for every
        // state would tell the phone a verification the user had just cleared), still with
        // the shown key.
        assert!(set(&db, &acct, &user(ada), false, &fp(&theirs), 52) == SetOutcome::Done);
        let job = sync_jobs(&db).remove(0);
        let content = db
            .read(|c| verified_sync_content(c, &job))
            .expect("read")
            .expect("a sync");
        let Some(crate::proto::content::Content::SyncMessage(sync)) = content.content else {
            panic!("the job must send a sync message");
        };
        let Some(crate::proto::sync_message::Content::Verified(v)) = sync.content else {
            panic!("the sync must be a Verified");
        };
        assert!(
            v.state == Some(crate::proto::verified::State::Default as i32),
            "after a Clear the state on the wire is DEFAULT"
        );
        assert!(
            v.identity_key.as_deref() == Some(&theirs.serialize()[..]),
            "the Clear still carries the shown key"
        );
    }

    /// The lead's R3 belt: the runner sends only what the stored row still says. A state that
    /// moved since the click (here: another device's click applied to the same key) is not
    /// told to the phone, even though the key is still the stored one.
    #[test]
    fn a_sync_whose_state_moved_since_the_click_is_dropped() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_DEFAULT);
        assert!(set(&db, &acct, &user(ada), true, &fp(&theirs), 71) == SetOutcome::Done);
        let job = sync_jobs(&db).remove(0);
        assert!(
            db.read(|c| verified_sync_content(c, &job))
                .expect("read")
                .is_some(),
            "the click's own state is sent"
        );
        let sid = ServiceId::from(ada).service_id_string();
        db.zone(|tx| -> Result<usize> {
            Ok(tx.execute(
                "UPDATE identities SET verified = ?2 WHERE service_id = ?1",
                (&sid, IDENTITY_DEFAULT),
            )?)
        })
        .expect("state moved");
        assert!(
            db.read(|c| verified_sync_content(c, &job))
                .expect("read")
                .is_none(),
            "a state that is no longer the stored one must not be sent"
        );
    }

    /// The state a queued sync job would send.
    fn payload_state(job: &Job) -> i64 {
        serde_json::from_slice::<SyncPayload>(job.payload.as_deref().expect("a payload"))
            .expect("a valid payload")
            .state
    }

    /// The lead's R6 (storage review MEDIUM-3, critic H3): a Clear clicked while the Mark's job
    /// runs makes the job run AGAIN with the second state (`VerifiedSync` re-runs on request).
    /// Without it `finish` deletes the row and the phone keeps VERIFIED while Kourier shows
    /// DEFAULT, with no error anywhere.
    #[test]
    fn a_click_during_the_run_runs_the_sync_again_with_the_second_state() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let (_dir, db) = db_with(ada, &theirs, crate::store::IDENTITY_DEFAULT);
        assert!(set(&db, &acct, &user(ada), true, &fp(&theirs), 61) == SetOutcome::Done);
        // The runner claims the Mark's job (it reads its payload now) ...
        let claimed = db
            .read(|c| jobs::next_due(c, 1_000, true, false))
            .expect("read")
            .expect("the Mark's job is due");
        assert!(payload_state(&claimed) == IDENTITY_VERIFIED, "the Mark");
        // ... and during its send he clicks Clear Verification.
        assert!(set(&db, &acct, &user(ada), false, &fp(&theirs), 62) == SetOutcome::Done);
        db.zone(|tx| jobs::finish(tx, &claimed, Done::Finished, 1_000))
            .expect("finish");
        let again = db
            .read(|c| jobs::next_due(c, 1_000, true, false))
            .expect("read")
            .expect("the job must still be queued and due after the run that read the Mark");
        assert!(
            payload_state(&again) == IDENTITY_DEFAULT,
            "the second run carries the Clear"
        );
        // The second run ends without a newer click: the row goes.
        db.zone(|tx| jobs::finish(tx, &again, Done::Finished, 1_001))
            .expect("finish");
        assert!(
            db.read(|c| jobs::next_due(c, 10_000, true, false))
                .expect("read")
                .is_none(),
            "nothing is left after the run that read the last click"
        );
    }

    /// Critic F16: a re-ask with the same keys and state answers from memory; their new key,
    /// our new key or a new state is a miss.
    #[test]
    fn the_memo_answers_only_for_the_same_keys_and_state() {
        let acct = crate::db::test_account();
        let ada = aci("796abedb-ca4e-4f18-8803-1fde5b921f9f");
        let theirs = key(BOB_IDENTITY);
        let found = Lookup::Found {
            aci: ada,
            key: theirs,
            verified: 0,
        };
        let memo = Memo::default();
        assert!(
            memo.hit(&acct, &found).is_none(),
            "nothing is remembered yet"
        );
        let number = answer(&acct, found.clone())
            .expect("answer")
            .expect("number");
        memo.remember(&acct, &found, &number);
        assert!(
            memo.hit(&acct, &found).as_ref() == Some(&number),
            "the same keys and state answer from memory"
        );
        let other_key = Lookup::Found {
            aci: ada,
            key: key(ALICE_IDENTITY),
            verified: 0,
        };
        assert!(
            memo.hit(&acct, &other_key).is_none(),
            "their new key is a miss"
        );
        let marked = Lookup::Found {
            aci: ada,
            key: theirs,
            verified: 1,
        };
        assert!(memo.hit(&acct, &marked).is_none(), "a new state is a miss");
        let other_account = crate::db::test_account();
        assert!(
            memo.hit(&other_account, &found).is_none(),
            "our new key is a miss"
        );
    }
}
