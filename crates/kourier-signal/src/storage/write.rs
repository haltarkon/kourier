//! The plan of a Storage Service write (A10b chunk B5; `target/next/a10-refresh.md` §3.4 step 5,
//! §3.5, §3.6 and the critic's X1, X4, M7, L2): which records change, as which bytes under which
//! new identifiers, and what the new manifest lists. PURE: nothing here reads the database (but
//! [`load_stored`]) or the network, and nothing is sent; the write job (A10c) gathers the inputs
//! in one zone, calls [`plan`], stores the result in the in-flight row and only then sends it.
//!
//! ## What goes in
//! - The FETCHED manifest (decrypted `ManifestRecord`): its identifiers are the only complete list
//!   of what the phone keeps, failed and unknown-type identifiers included, so the new manifest is
//!   built from them and never from `storage_records` (BQ7: there is no `manifest_record`
//!   column).
//! - The stored raw records by target ([`StoredRecord`], [`load_stored`]), the live intents, the
//!   bases the core was told (`intents::told_archived`), the pin limit and our device id.
//!
//! ## What comes out ([`Plan`])
//! - Per touched target ONE new record: the bytes of the newest listed stored record edited with
//!   the wire editor (`wire.rs`: every byte the plan does not change is copied), under a NEW random
//!   16-byte identifier; the old identifier is deleted. A record is never re-encoded from the prost
//!   struct. More than one listed record of a target (Desktop's `checkForConflicts`): the newest
//!   is rotated and the others are deleted, as Desktop does on its next write (M7).
//! - The manifest: the fetched identifiers minus the deleted ones plus the inserted ones, in that
//!   order, each kept identifier exactly as fetched (an unknown type stays), `version` = the
//!   fetched one + 1, `sourceDevice` = ours, the fetched `recordIkm` as it was (ABSENT stays absent:
//!   a linked device never generates one, L2; the record keys are then the legacy ones).
//! - [`Plan::planned`]: per `(target, field)` the value written and the `seq` of the intent it
//!   closes, the list the in-flight row stores (X4: the landing zone after a kill has no plaintext,
//!   only this list), and X1's ride-along `archived` of each rotated record (`rides`, `seq` 0:
//!   no intent asked for it, but the write sets it all the same; review H1).
//! - [`Plan::left`]: the intents the plan did not write and why. A refused plan has none of this.
//!
//! ## What is written (and what never is)
//! - The intents' fields: `archived`, `markedUnread`, `mutedUntilTimestamp` of a contact and of a
//!   group record, `noteToSelfArchived` and `noteToSelfMarkedUnread` of the account record, the
//!   pin list (field 14: our removals dropped, our additions after the last entry in `seq` order,
//!   every other entry byte for byte), `identityState` of a contact record whose key IS the
//!   intent's key (M2: another key, or none, is not written; the intent is `Why::OtherKey`).
//! - X1: when a contact, group or account record is rotated for ANY reason, `archived` (11 / 4 /
//!   Note to Self 5) is written from the base the core was last told (`peers.roaming_archived`,
//!   `groups.archived`) unless an archive intent covers it: A1's local unarchive (a message brought
//!   the chat back; the stored record still says archived) rides along with the next write, as on
//!   Android, and the phone does not re-archive a chat it shows in its inbox. A target with no told
//!   base keeps the record's flag. The ride-along is carried in [`Plan::planned`] like a field an
//!   intent sets (review H1).
//! - Never written: a record that does not decode (H6: it is not edited, its intents are left,
//!   `Why::Unreadable`), a target no listed record exists for (`Why::NotListed`), a kind of record
//!   the engine does not edit, a PNI-only contact's pin, `identityKey`, nicknames, blocks, or any
//!   field of a record no intent names. The manifest's unknown fields are not kept (the prost
//!   struct has none; Desktop builds a fresh manifest too).
//!
//! ## Validation ([`validate`], H6)
//! The phone's worst case is a force push: Android makes the primary rewrite ALL of storage when a
//! linked device's write has an identifier whose type is not its record's, or a record that does
//! not decrypt. [`plan`] ends with [`validate`] and returns `Err` for: a type mismatch, a duplicate
//! identifier, a second account record (the fetched manifest's two are kept while the write does
//! not touch the account record; a write that rotates it lists one), an insert the manifest does
//! not list, a delete it still lists, a version other than the fetched one + 1, a changed
//! `recordIkm`, an inserted identifier that is not 16 bytes, and a manifest that is not the
//! fetched one minus the deletes plus the inserts. A refused plan sends nothing and keeps the
//! intents (the job repeats with its backoff).
//!
//! The write job itself, what fetches, plans, sends and lands: [`run`] (`write/job.rs`).
//!
//! The words are Storage Service words on purpose: this module is in `kourier-signal`. Debug
//! output of every type here carries counts and kinds only (architecture §9.1).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use kourier_db::rusqlite::Connection;
use prost::Message as _;
use zeroize::Zeroizing;

use super::ACCOUNT_TARGET;
use super::intents::{
    self, ARCHIVED, IDENTITY, Intent, MARKED_UNREAD, MUTED_UNTIL, PIN_PREFIX, Planned,
};
use super::wire::{self, WireError, num};
use crate::error::{Error, Result};
use crate::proto::{
    self, account_record::PinnedConversation, manifest_record::Identifier,
    manifest_record::identifier::Type as ItemType, storage_record::Record,
};

/// Why a plan is refused. Fixed words, log-safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Refusal {
    /// An identifier's type is not the type of its record (the phone force-pushes on that).
    #[error("an identifier's type is not its record's type")]
    TypeMismatch,
    /// An identifier is listed twice.
    #[error("an identifier is listed twice")]
    DuplicateId,
    /// The manifest would list more than one account record, and not as the phone's manifest
    /// already did with the account record untouched.
    #[error("the manifest would list more than one account record")]
    TwoAccountRecords,
    /// A record to insert has no identifier in the manifest.
    #[error("a record to insert is not listed")]
    InsertNotListed,
    /// A record to delete is still listed.
    #[error("a record to delete is still listed")]
    DeleteListed,
    /// The manifest's version is not the fetched one + 1.
    #[error("the manifest's version is not the fetched one plus one")]
    Version,
    /// The manifest's `recordIkm` is not the fetched one.
    #[error("the manifest's record key material changed")]
    RecordIkm,
    /// A new identifier is not 16 bytes.
    #[error("a new identifier is not 16 bytes")]
    BadId,
    /// The manifest is not the fetched one minus the deletes plus the inserts.
    #[error("the manifest is not the fetched one minus the deletes plus the inserts")]
    Drift,
}

/// Why an intent is not in the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// The fetched manifest lists no stored record of the target (the phone removed it, or the
    /// merge of the fetched manifest has not run yet): keep the intent, the merge decides.
    NotListed,
    /// The newest listed record does not decode or cannot be walked: never edited (H6). Keep the
    /// intent.
    Unreadable,
    /// This kind of record cannot take the field (a mute on Note to Self, a pin of a person known
    /// only by a PNI, a field name this build does not know): the intent can never be written.
    NotWritable,
    /// A safety-number state for another key than the record's (M2): dropped, nothing stated.
    OtherKey,
    /// The pin list would pass the limit after the rebase: our newest addition is dropped (M9 b).
    OverPinLimit,
}

impl Why {
    /// Whether the intent stays for a later record to decide (`NotListed`, `Unreadable`); the
    /// others can never be written and are deleted ([`intents::drop_left`]).
    pub(crate) fn keeps_the_intent(self) -> bool {
        matches!(self, Self::NotListed | Self::Unreadable)
    }
}

/// An intent the plan did not write.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Left {
    pub(crate) target: String,
    pub(crate) field: String,
    pub(crate) seq: i64,
    pub(crate) why: Why,
}

impl fmt::Debug for Left {
    /// Log-safe: no target (it names a chat).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.field.split(':').next().unwrap_or("");
        f.debug_struct("Left")
            .field("field", &kind)
            .field("seq", &self.seq)
            .field("why", &self.why)
            .finish_non_exhaustive()
    }
}

/// One stored raw record the plan may edit (`storage_records`, filled by `fill_targets`).
pub(crate) struct StoredRecord {
    /// The record's raw identifier.
    pub(crate) key: Vec<u8>,
    /// `contact:<sid>`, `group:<hex>` or `account`.
    pub(crate) target: String,
    /// `storage_records.peer` (copied to the record that replaces this one).
    pub(crate) peer: Option<String>,
    /// The decrypted `StorageRecord`, as the phone wrote it.
    pub(crate) raw: Zeroizing<Vec<u8>>,
    /// When it was merged (the backend's clock).
    pub(crate) updated_at: i64,
    /// `storage_records.rowid`: the order of two rows merged in the same millisecond.
    pub(crate) rowid: i64,
}

impl fmt::Debug for StoredRecord {
    /// Log-safe: sizes only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredRecord")
            .field("len", &self.raw.len())
            .finish_non_exhaustive()
    }
}

/// Everything [`plan`] reads.
pub(crate) struct Input<'a> {
    /// The manifest fetched for this write (its `version` is the remote one).
    pub(crate) fetched: &'a proto::ManifestRecord,
    /// The stored records of the touched targets ([`load_stored`]); other rows are ignored.
    pub(crate) stored: &'a [StoredRecord],
    /// Every live intent (`intents::all_intents`).
    pub(crate) intents: &'a [Intent],
    /// What the core was last told of each target's archive state (`intents::told_archived`).
    pub(crate) told_archived: &'a HashMap<String, bool>,
    /// The pin limit of the account (`intents::pin_limit_of`).
    pub(crate) pin_limit: u32,
    /// Our device id, the new manifest's `sourceDevice`.
    pub(crate) device_id: u32,
}

/// A record to put.
pub(crate) struct Insert {
    /// The NEW identifier (16 random bytes).
    pub(crate) key: Vec<u8>,
    /// The identifier's type (the record's oneof).
    pub(crate) kind: ItemType,
    /// What the record belongs to.
    pub(crate) target: String,
    /// `storage_records.peer` of the record it replaces.
    pub(crate) peer: Option<String>,
    /// The `StorageRecord` bytes to encrypt and store.
    pub(crate) record: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for Insert {
    /// Log-safe: kind and size only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Insert")
            .field("kind", &self.kind)
            .field("len", &self.record.len())
            .finish_non_exhaustive()
    }
}

/// A write, ready to be stored in the in-flight row and sent.
pub(crate) struct Plan {
    /// The manifest to send: `version` = remote + 1.
    pub(crate) manifest: proto::ManifestRecord,
    /// The records to insert, in manifest order.
    pub(crate) inserts: Vec<Insert>,
    /// The raw identifiers to delete: the records replaced and their duplicates.
    pub(crate) deletes: Vec<Vec<u8>>,
    /// What the write carries per `(target, field)`, in `seq` order: X1's ride-alongs (`seq` 0)
    /// first, then the intents, oldest first.
    pub(crate) planned: Vec<Planned>,
    /// The intents that are not in the write.
    pub(crate) left: Vec<Left>,
}

impl fmt::Debug for Plan {
    /// Log-safe: counts only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plan")
            .field("version", &self.manifest.version)
            .field("identifiers", &self.manifest.identifiers.len())
            .field("inserts", &self.inserts.len())
            .field("deletes", &self.deletes.len())
            .field("planned", &self.planned.len())
            .field("left", &self.left.len())
            .finish_non_exhaustive()
    }
}

impl Plan {
    /// Nothing to write (every intent is left): the job must not send it (a PUT would only move
    /// the manifest's version).
    pub(crate) fn is_empty(&self) -> bool {
        self.inserts.is_empty()
    }

    /// The `planned` column of the in-flight row: a JSON list of objects
    /// `{"target", "field", "value", "seq"}` (+ `"rides": true`, `"overtaken": true` and
    /// `"prior"` when set), `value` and `prior` integers or `null` (unmuted), the shape `intents`
    /// reads back (`intents::Planned`). What [`write_in_flight`] stores; the tests read it here.
    #[cfg(test)]
    pub(crate) fn planned_json(&self) -> Result<String> {
        serde_json::to_string(&self.planned)
            .map_err(|_| Error::Decode("the planned list of a Storage Service write"))
    }
}

// ---------------------------------------------------------------------------------------------
// Inputs from the database
// ---------------------------------------------------------------------------------------------

/// The distinct targets the intents name, in a fixed order.
pub(crate) fn touched_targets(intents: &[Intent]) -> Vec<String> {
    intents
        .iter()
        .map(|i| i.target.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The stored records of `targets` (call `fill_targets` first, in the same zone).
pub(crate) fn load_stored(conn: &Connection, targets: &[String]) -> Result<Vec<StoredRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT key, peer, record, updated_at, rowid FROM storage_records WHERE target = ?1",
    )?;
    let mut out = Vec::new();
    for target in targets {
        let rows = stmt.query_map([target], |r| {
            Ok(StoredRecord {
                key: r.get(0)?,
                target: target.clone(),
                peer: r.get(1)?,
                raw: Zeroizing::new(r.get(2)?),
                updated_at: r.get(3)?,
                rowid: r.get(4)?,
            })
        })?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// A new random record identifier: 16 bytes, like Desktop's and Android's.
pub(crate) fn random_record_id() -> [u8; 16] {
    use rand::RngCore as _;
    let mut id = [0u8; 16];
    rand::rng().fill_bytes(&mut id);
    id
}

// ---------------------------------------------------------------------------------------------
// The write in flight (A10b chunk B6; §3.6, critic X3, X4, Y5)
// ---------------------------------------------------------------------------------------------

/// The row of `storage_write_inflight`: a write that was planned and may have reached the server.
/// At most one exists. It is written in the zone that decides to send the write (before the PUT),
/// it decides after a kill ([`landed`]) and it carries what the landing zone must close
/// (`intents::land`): the plaintext of the records is NOT kept (X4), only the list of what the
/// write carries per `(target, field)` and the new identifiers. Deleted by the landing zone, by a
/// write found not landed (a sync, A10c's non-2xx), by `storage::reset`, and by any reader that
/// finds it unreadable ([`find_in_flight`], review L3).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct InFlight {
    /// The manifest version the write sets (the fetched one + 1).
    pub(crate) version: u64,
    /// The raw identifiers the write inserts (16 bytes each).
    pub(crate) new_keys: Vec<Vec<u8>>,
    /// What the write carries per `(target, field)`: the value and the `seq` of the intent it
    /// closes (Y5).
    pub(crate) planned: Vec<Planned>,
    /// `account.storage_generation` when it was planned: a landing zone whose generation moved
    /// since (a `storage::reset` ran) drops its result (M4).
    pub(crate) generation: i64,
    /// When it was planned (the backend's clock).
    pub(crate) planned_at: i64,
}

impl fmt::Debug for InFlight {
    /// Log-safe: counts only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InFlight")
            .field("version", &self.version)
            .field("new_keys", &self.new_keys.len())
            .field("planned", &self.planned.len())
            .finish_non_exhaustive()
    }
}

impl InFlight {
    /// The row for `plan`, planned under `generation` at `now`.
    pub(crate) fn of(plan: &Plan, generation: i64, now: i64) -> Self {
        Self {
            version: plan.manifest.version,
            new_keys: plan.inserts.iter().map(|i| i.key.clone()).collect(),
            planned: plan.planned.clone(),
            generation,
            planned_at: now,
        }
    }
}

/// Writes the in-flight row. A row that exists already is an error, not replaced: it may stand
/// for a write that reached the server, and only [`landed`] / the landing zone may retire it.
pub(crate) fn write_in_flight(conn: &Connection, row: &InFlight) -> Result<()> {
    let planned = serde_json::to_string(&row.planned)
        .map_err(|_| Error::Decode("the planned list of a Storage Service write"))?;
    let version = i64::try_from(row.version)
        .map_err(|_| Error::Decode("the version of a Storage Service write"))?;
    conn.prepare_cached(
        "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation, planned_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5)",
    )?
    .execute((
        version,
        row.new_keys.concat(),
        planned,
        row.generation,
        row.planned_at,
    ))?;
    Ok(())
}

/// What the in-flight table holds.
#[derive(Debug)]
pub(crate) enum Row {
    /// No write is in flight.
    None,
    /// The row, as this build reads it.
    Readable(InFlight),
    /// A row this build cannot read (its `planned` JSON, its identifiers or a column): one warn
    /// line (no content) and the row is DELETED. The write counts as not landed (review L3): the
    /// intents stay and are planned again. The deletion is already done when this is returned.
    Unreadable,
}

/// The columns of the in-flight row, each read on its own so that one of another type is an
/// unreadable row and not an error.
type RawRow = (
    kourier_db::rusqlite::Result<i64>,
    kourier_db::rusqlite::Result<Vec<u8>>,
    kourier_db::rusqlite::Result<String>,
    kourier_db::rusqlite::Result<i64>,
    kourier_db::rusqlite::Result<i64>,
);

/// The in-flight row, if there is one, with an unreadable one deleted and logged ([`Row`]). Never
/// an `Error::Decode`: that error is permanent for a job, and a row a later build cannot read
/// would stop every sync, fail every click and the receive zone of a message (review L3). Real
/// database errors still are errors. It deletes, so it belongs in a write zone (every caller is
/// one; `Db::read` is query-only).
pub(crate) fn find_in_flight(conn: &Connection) -> Result<Row> {
    use kourier_db::rusqlite::OptionalExtension as _;
    let row: Option<RawRow> = conn
        .prepare_cached(
            "SELECT version, new_keys, planned, generation, planned_at
             FROM storage_write_inflight WHERE id = 1",
        )?
        .query_row([], |r| {
            Ok((r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        })
        .optional()?;
    let Some(raw) = row else {
        return Ok(Row::None);
    };
    match parse_row(raw) {
        Some(row) => Ok(Row::Readable(row)),
        None => {
            tracing::warn!(
                "the storage write in flight cannot be read by this build: counted as not landed and deleted"
            );
            delete_in_flight(conn)?;
            Ok(Row::Unreadable)
        }
    }
}

/// The columns of the in-flight row as an [`InFlight`]; `None` when any is not what this build
/// reads.
fn parse_row(raw: RawRow) -> Option<InFlight> {
    let (Ok(version), Ok(new_keys), Ok(planned), Ok(generation), Ok(planned_at)) = raw else {
        return None;
    };
    if new_keys.len() % 16 != 0 {
        return None;
    }
    Some(InFlight {
        version: u64::try_from(version).ok()?,
        new_keys: new_keys
            .as_chunks::<16>()
            .0
            .iter()
            .map(|k| k.to_vec())
            .collect(),
        planned: serde_json::from_str(&planned).ok()?,
        generation,
        planned_at,
    })
}

/// The in-flight row, if there is one and this build can read it (an unreadable one is deleted
/// and logged, [`find_in_flight`]).
pub(crate) fn read_in_flight(conn: &Connection) -> Result<Option<InFlight>> {
    Ok(match find_in_flight(conn)? {
        Row::Readable(row) => Some(row),
        Row::None | Row::Unreadable => None,
    })
}

/// Whether a row is in flight (readable or not): `storage::sync` asks before it opens a zone to
/// judge it (review N1 (a)), so a sync with no write in flight opens no zone more than before.
pub(crate) fn in_flight_exists(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM storage_write_inflight)")?
        .query_row([], |r| r.get(0))?)
}

/// Deletes the in-flight row (a no-op when there is none).
pub(crate) fn delete_in_flight(conn: &Connection) -> Result<()> {
    conn.prepare_cached("DELETE FROM storage_write_inflight")?
        .execute([])?;
    Ok(())
}

/// A message brought the chat of `target` back while the write in flight carries an `archived`
/// entry for it (`intents::chat_left_the_archive`): marks EVERY such entry [`Planned::overtaken`]
/// in the row, whatever it sets (review R2-2 (1): an Unarchive's 0 and a ride-along too), so a
/// landing with the switch off does not state it (review L1) and a write that does not land does
/// not restate its `prior`. No row, or no such entry: nothing.
pub(crate) fn mark_overtaken(conn: &Connection, target: &str) -> Result<()> {
    let Some(mut row) = read_in_flight(conn)? else {
        return Ok(());
    };
    let mut marked = false;
    for planned in &mut row.planned {
        if planned.target == target && planned.field == ARCHIVED && !planned.overtaken {
            planned.overtaken = true;
            marked = true;
        }
    }
    if marked {
        let json = serde_json::to_string(&row.planned)
            .map_err(|_| Error::Decode("the planned list of a Storage Service write"))?;
        conn.prepare_cached("UPDATE storage_write_inflight SET planned = ?1 WHERE id = 1")?
            .execute([json])?;
    }
    Ok(())
}

/// D2: [`intents::drop_where`] deleted a newer click on each carried `(target, field)` of `fields`
/// WITHOUT restating the phone's value, because the landing was to state the written one: marks
/// those entries [`Planned::state_at_landing`] in the row, in the zone that deleted the click, so
/// the promise outlives the switch (a landing, or a write that did not land, keeps it whatever the
/// switch says by then). No row, or no such entry: nothing.
pub(crate) fn mark_state_at_landing(conn: &Connection, fields: &[(&str, &str)]) -> Result<()> {
    let Some(mut row) = read_in_flight(conn)? else {
        return Ok(());
    };
    let mut marked = false;
    for planned in &mut row.planned {
        if !planned.state_at_landing
            && fields
                .iter()
                .any(|(target, field)| planned.target == *target && planned.field == *field)
        {
            planned.state_at_landing = true;
            marked = true;
        }
    }
    if marked {
        let json = serde_json::to_string(&row.planned)
            .map_err(|_| Error::Decode("the planned list of a Storage Service write"))?;
        conn.prepare_cached("UPDATE storage_write_inflight SET planned = ?1 WHERE id = 1")?
            .execute([json])?;
    }
    Ok(())
}

/// What [`store_written`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Written {
    /// The written records replace the old ones and `manifest_version` is the written version.
    Stored,
    /// The written records are stored, but `manifest_version` stays: the write was planned on a
    /// manifest newer than the merged one, and the next sync merges what lies between (it finds
    /// ours stored and reads only the phone's records).
    StoredBehind,
    /// Nothing stored: a sync already merged this version or a newer one (the records are there,
    /// or the phone's newer manifest replaced them).
    AlreadyMerged,
    /// Nothing stored: the plan is not the write in flight (a caller's mistake; one warn line).
    NotTheRow,
}

/// The 200 path's storage half (review B1, the lead's call): what the write put on the server is
/// stored as the phone's next sync would have merged it: the rows of [`Plan::deletes`] go, each
/// [`Plan::inserts`] is a row (key, kind, peer, target, the written bytes, `updated_at = now`,
/// `ours = 1`: review N6),
/// and `manifest_version` becomes the written version. Nothing else changes; the intents are the
/// landing's (`intents::land_written` calls this, then `intents::land`). `manifest_version` never
/// moves without the records, never moves back, and never jumps over a version whose records were
/// not merged.
pub(crate) fn store_written(
    conn: &Connection,
    row: &InFlight,
    plan: &Plan,
    now: i64,
) -> Result<Written> {
    use kourier_db::rusqlite::OptionalExtension as _;
    let same_keys = row.new_keys.len() == plan.inserts.len()
        && row
            .new_keys
            .iter()
            .zip(&plan.inserts)
            .all(|(key, insert)| *key == insert.key);
    if row.version != plan.manifest.version || !same_keys {
        tracing::warn!(
            inserts = plan.inserts.len(),
            ids = row.new_keys.len(),
            "a storage write's plan is not the write in flight: its records are not stored"
        );
        return Ok(Written::NotTheRow);
    }
    let version = i64::try_from(row.version)
        .map_err(|_| Error::Decode("the version of a Storage Service write"))?;
    let merged: Option<i64> = conn
        .prepare_cached("SELECT manifest_version FROM account WHERE id = 1")?
        .query_row([], |r| r.get(0))
        .optional()?
        .flatten();
    if merged.is_some_and(|merged| merged >= version) {
        return Ok(Written::AlreadyMerged);
    }
    for key in &plan.deletes {
        conn.prepare_cached("DELETE FROM storage_records WHERE key = ?1")?
            .execute([key])?;
    }
    for insert in &plan.inserts {
        // `ours = 1`: a record OUR write put there (review N6): the automatic clear's N-C does not
        // count it as the phone's newer mark.
        conn.prepare_cached(
            "INSERT INTO storage_records(key, kind, peer, target, record, updated_at, ours)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)
             ON CONFLICT(key) DO UPDATE SET kind = excluded.kind, peer = excluded.peer,
               target = excluded.target, record = excluded.record,
               updated_at = excluded.updated_at, ours = excluded.ours",
        )?
        .execute((
            &insert.key,
            insert.kind as i32,
            &insert.peer,
            &insert.target,
            insert.record.as_slice(),
            now,
        ))?;
    }
    if merged.is_some_and(|merged| merged.checked_add(1) == Some(version)) {
        conn.prepare_cached("UPDATE account SET manifest_version = ?1 WHERE id = 1")?
            .execute([version])?;
        Ok(Written::Stored)
    } else {
        Ok(Written::StoredBehind)
    }
}

/// Whether a write found in flight reached the server, judged from a freshly FETCHED manifest
/// (§3.6, BQ9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landed {
    /// The write did not land (the server is behind it, or the phone wrote the version we meant to
    /// write, or the manifest moved on and still lists an identifier our write deleted): the row
    /// is deleted and the write is planned again.
    No,
    /// It landed: one of our new identifiers is listed at the write's version or at a later one.
    Yes,
    /// The manifest moved past our version, lists none of our identifiers and none of the
    /// identifiers our write deleted (the phone wrote again before we looked, and replaced our
    /// records): COUNTED AS LANDED. A click lost is preferred to undoing what the phone did since;
    /// the caller logs a count.
    Presumed,
}

impl Landed {
    /// Whether the landing zone runs.
    pub(crate) fn is_landed(self) -> bool {
        self != Self::No
    }
}

/// The identifiers a write in flight DELETED, as far as the stored records tell (D1): the keys of
/// the stored records of every target the row carries (a pin, Note to Self's flags and X1's
/// ride-alongs name the account record) that are not newer than the row (`updated_at <=
/// planned_at`). The plan deleted exactly the listed stored records of those targets (M7) and they
/// stay stored until the landing or a merge removes them, so call this BEFORE any merge zone stores
/// the manifest the row is judged by; a record merged after the plan is the phone's, not ours to
/// have deleted. The row does not keep the deleted identifiers (no schema change); [`landed`] takes
/// them as an argument and stays pure.
pub(crate) fn deleted_ids(conn: &Connection, row: &InFlight) -> Result<Vec<Vec<u8>>> {
    let targets: BTreeSet<&str> = row.planned.iter().map(|p| p.target.as_str()).collect();
    let mut stmt = conn
        .prepare_cached("SELECT key FROM storage_records WHERE target = ?1 AND updated_at <= ?2")?;
    let mut out = Vec::new();
    for target in targets {
        for key in stmt.query_map((target, row.planned_at), |r| r.get::<_, Vec<u8>>(0))? {
            out.push(key?);
        }
    }
    Ok(out)
}

/// THE landing rule (§3.6, the lead's BQ9). Versions: `fetched < row` is not landed; `fetched ==
/// row` is landed only when one of our new identifiers is listed (otherwise the phone wrote that
/// version and our write must go again); `fetched > row` is landed when one is listed, and
/// otherwise PRESUMED landed (the phone wrote again and replaced our records) unless an identifier
/// our write DELETED (`deleted`, [`deleted_ids`]) is still listed (D1). A Storage Service write is
/// atomic, so a listed deleted identifier means the write was not applied (or the phone put its own
/// record back under the old identifier): writing again is right, or a harmless repeat of a value
/// the phone has. Without this check a PUT that never reached the server, followed by two writes of
/// the phone, was closed as landed and left the click shown here for good (the crash table's row
/// "not applied, then the phone wrote twice or more").
pub(crate) fn landed(
    fetched: &proto::ManifestRecord,
    row: &InFlight,
    deleted: &[Vec<u8>],
) -> Landed {
    if fetched.version < row.version {
        return Landed::No;
    }
    let ours: HashSet<&[u8]> = row.new_keys.iter().map(Vec::as_slice).collect();
    let listed = fetched
        .identifiers
        .iter()
        .any(|i| ours.contains(i.raw.as_slice()));
    let deleted_still_listed = || {
        let deleted: HashSet<&[u8]> = deleted.iter().map(Vec::as_slice).collect();
        fetched
            .identifiers
            .iter()
            .any(|i| deleted.contains(i.raw.as_slice()))
    };
    match (fetched.version == row.version, listed) {
        (_, true) => Landed::Yes,
        (true, false) => Landed::No,
        (false, false) if deleted_still_listed() => Landed::No,
        (false, false) => Landed::Presumed,
    }
}

// ---------------------------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------------------------

/// The three kinds of record the engine edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Contact,
    Group,
    Account,
}

/// The field numbers of a kind of record.
struct Layout {
    /// `StorageRecord`'s oneof field.
    record: u32,
    archived: u32,
    marked_unread: u32,
    /// `None`: no mute (Note to Self).
    muted_until: Option<u32>,
}

impl Shape {
    fn of(target: &str) -> Option<Self> {
        if target == ACCOUNT_TARGET {
            Some(Self::Account)
        } else if target.starts_with("contact:") {
            Some(Self::Contact)
        } else if target.starts_with("group:") {
            Some(Self::Group)
        } else {
            None
        }
    }

    fn item_type(self) -> ItemType {
        match self {
            Self::Contact => ItemType::Contact,
            Self::Group => ItemType::Groupv2,
            Self::Account => ItemType::Account,
        }
    }

    fn layout(self) -> Layout {
        match self {
            Self::Contact => Layout {
                record: num::RECORD_CONTACT,
                archived: num::CONTACT_ARCHIVED,
                marked_unread: num::CONTACT_MARKED_UNREAD,
                muted_until: Some(num::CONTACT_MUTED_UNTIL),
            },
            Self::Group => Layout {
                record: num::RECORD_GROUP_V2,
                archived: num::GROUP_ARCHIVED,
                marked_unread: num::GROUP_MARKED_UNREAD,
                muted_until: Some(num::GROUP_MUTED_UNTIL),
            },
            Self::Account => Layout {
                record: num::RECORD_ACCOUNT,
                archived: num::ACCOUNT_NOTE_TO_SELF_ARCHIVED,
                marked_unread: num::ACCOUNT_NOTE_TO_SELF_MARKED_UNREAD,
                muted_until: None,
            },
        }
    }

    /// The order the merges use: contacts, groups, the account record (pins name the others) last.
    fn rank(self) -> u8 {
        match self {
            Self::Contact => 0,
            Self::Group => 1,
            Self::Account => 2,
        }
    }
}

/// The identifier type a record's oneof gives it; `None` for a kind the engine does not write.
fn oneof_type(record: &proto::StorageRecord) -> Option<ItemType> {
    match &record.record {
        Some(Record::Contact(_)) => Some(ItemType::Contact),
        Some(Record::GroupV2(_)) => Some(ItemType::Groupv2),
        Some(Record::Account(_)) => Some(ItemType::Account),
        _ => None,
    }
}

/// A 0/1 field's value from an intent.
fn flag(intent: &Intent) -> u64 {
    u64::from(intent.value == Some(1))
}

/// The mute's end as the record's `uint64`: unmuted (`None`) and anything below 1 is 0.
fn mute_ms(intent: &Intent) -> u64 {
    intent
        .value
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0)
}

/// What one target's rotation came to.
struct Rotation {
    /// The edited `StorageRecord`; `None` when no intent could be written.
    bytes: Option<Vec<u8>>,
    written: Vec<Planned>,
    left: Vec<Left>,
}

fn left_of(intent: &Intent, why: Why) -> Left {
    Left {
        target: intent.target.clone(),
        field: intent.field.clone(),
        seq: intent.seq,
        why,
    }
}

fn planned_of(intent: &Intent) -> Planned {
    Planned {
        target: intent.target.clone(),
        field: intent.field.clone(),
        value: intent.value,
        seq: intent.seq,
        rides: false,
        overtaken: false,
        // The phone's value before the write (review N1 (b)): what the intent was made against.
        prior: intent.base,
        state_at_landing: false,
    }
}

/// Edits one record for the intents of its target (`mine`, oldest first). `target` names it in
/// what the write carries.
fn rotate(
    shape: Shape,
    target: &str,
    raw: &[u8],
    decoded: &proto::StorageRecord,
    mine: &[&Intent],
    told_archived: Option<bool>,
    pin_limit: u32,
) -> std::result::Result<Rotation, WireError> {
    let layout = shape.layout();
    let mut sets: Vec<(u32, u64)> = Vec::new();
    let mut written: Vec<Planned> = Vec::new();
    let mut left: Vec<Left> = Vec::new();
    let mut archive_covered = false;
    // The pin list: chats to take out, entries to add (the intent, the pin-list name, the bytes).
    let mut removed: Vec<String> = Vec::new();
    let mut added: Vec<(&Intent, String, Vec<u8>)> = Vec::new();
    let listed: Vec<Option<String>> = match &decoded.record {
        Some(Record::Account(account)) => account
            .pinned_conversations
            .iter()
            .map(intents::entry_target)
            .collect(),
        _ => Vec::new(),
    };

    for intent in mine {
        let intent: &Intent = intent;
        match intent.field.as_str() {
            ARCHIVED => {
                sets.push((layout.archived, flag(intent)));
                archive_covered = true;
                written.push(planned_of(intent));
            }
            MARKED_UNREAD => {
                sets.push((layout.marked_unread, flag(intent)));
                written.push(planned_of(intent));
            }
            MUTED_UNTIL => match layout.muted_until {
                Some(number) => {
                    sets.push((number, mute_ms(intent)));
                    written.push(planned_of(intent));
                }
                None => left.push(left_of(intent, Why::NotWritable)),
            },
            IDENTITY => match &decoded.record {
                Some(Record::Contact(contact)) => {
                    let same_key = intent
                        .key
                        .as_deref()
                        .is_some_and(|k| !k.is_empty() && k == contact.identity_key.as_slice());
                    if same_key {
                        let state = intent.value.and_then(|v| u64::try_from(v).ok());
                        sets.push((num::CONTACT_IDENTITY_STATE, state.unwrap_or(0)));
                        written.push(planned_of(intent));
                    } else {
                        left.push(left_of(intent, Why::OtherKey));
                    }
                }
                _ => left.push(left_of(intent, Why::NotWritable)),
            },
            field => match field.strip_prefix(PIN_PREFIX) {
                Some(chat) if shape == Shape::Account => {
                    if intent.value != Some(1) {
                        removed.push(chat.to_owned());
                        written.push(planned_of(intent));
                    } else if listed.iter().any(|t| t.as_deref() == Some(chat)) {
                        // Already pinned on the phone: nothing to add, the intent is closed.
                        written.push(planned_of(intent));
                    } else if let Some(entry) = intents::new_entry_bytes(chat) {
                        added.push((intent, chat.to_owned(), entry));
                    } else {
                        left.push(left_of(intent, Why::NotWritable));
                    }
                }
                _ => left.push(left_of(intent, Why::NotWritable)),
            },
        }
    }

    // The pin limit after the rebase over what the phone pinned meanwhile (M9 b): our newest
    // addition goes first. The phone's own entries are never touched.
    let kept = listed
        .iter()
        .filter(|t| t.as_ref().is_none_or(|t| !removed.contains(t)))
        .count();
    while kept + added.len() > pin_limit as usize {
        let Some((intent, _, _)) = added.pop() else {
            break;
        };
        left.push(left_of(intent, Why::OverPinLimit));
    }
    for (intent, _, _) in &added {
        written.push(planned_of(intent));
    }

    if written.is_empty() {
        return Ok(Rotation {
            bytes: None,
            written,
            left,
        });
    }

    // X1: the told base rides along unless an archive intent covers the flag, and the write
    // carries it like a field it sets (review H1: a click during the PUT takes it as its base, the
    // landing moves a live archive intent's base to it, and a message that brings the chat back
    // after a written `1` is followed by `archived = false`).
    if !archive_covered && let Some(told) = told_archived {
        sets.push((layout.archived, u64::from(told)));
        written.push(Planned {
            target: target.to_owned(),
            field: ARCHIVED.to_owned(),
            value: Some(i64::from(told)),
            seq: 0,
            rides: true,
            overtaken: false,
            // The told base is also the phone's value of it before the write (review N1 (b)).
            prior: Some(i64::from(told)),
            state_at_landing: false,
        });
    }

    let bytes = wire::edit_message(raw, layout.record, |inner| {
        let mut current = inner.to_vec();
        for &(number, value) in &sets {
            current = wire::set_varint(&current, number, value)?;
        }
        if !removed.is_empty() {
            current = wire::retain_repeated(&current, num::ACCOUNT_PINNED, |entry| {
                match PinnedConversation::decode(entry) {
                    Ok(entry) => {
                        intents::entry_target(&entry).is_none_or(|t| !removed.contains(&t))
                    }
                    // An entry this build cannot read stays as it is.
                    Err(_) => true,
                }
            })?;
        }
        for (_, _, entry) in &added {
            current = wire::push_message(&current, num::ACCOUNT_PINNED, entry)?;
        }
        Ok(current)
    })?;
    written.sort_by_key(|p| p.seq);
    Ok(Rotation {
        bytes: Some(bytes),
        written,
        left,
    })
}

/// Plans the write for the intents of `input`. `new_id` gives the new record identifiers
/// ([`random_record_id`] in production). `Err` = refused (see [`validate`]); `Ok` with
/// [`Plan::is_empty`] = nothing can be written.
pub(crate) fn plan(
    input: &Input<'_>,
    new_id: &mut impl FnMut() -> [u8; 16],
) -> std::result::Result<Plan, Refusal> {
    let fetched = input.fetched;
    let listed: HashMap<&[u8], i32> = fetched
        .identifiers
        .iter()
        .map(|i| (i.raw.as_slice(), i.r#type))
        .collect();

    // The intents by target, the targets in merge order.
    let mut by_target: BTreeMap<&str, Vec<&Intent>> = BTreeMap::new();
    for intent in input.intents {
        by_target.entry(&intent.target).or_default().push(intent);
    }
    let mut targets: Vec<(&str, Vec<&Intent>)> = by_target.into_iter().collect();
    targets.sort_by_key(|(target, _)| Shape::of(target).map_or(3, Shape::rank));

    let mut inserts: Vec<Insert> = Vec::new();
    let mut deletes: Vec<Vec<u8>> = Vec::new();
    let mut planned: Vec<Planned> = Vec::new();
    let mut left: Vec<Left> = Vec::new();

    for (target, mut mine) in targets {
        mine.sort_by_key(|i| i.seq);
        let leave_all = |why: Why| mine.iter().map(|i| left_of(i, why)).collect::<Vec<_>>();
        let Some(shape) = Shape::of(target) else {
            left.extend(leave_all(Why::NotWritable));
            continue;
        };
        // The listed records of the target, the newest first (M7).
        let mut candidates: Vec<&StoredRecord> = input
            .stored
            .iter()
            .filter(|s| s.target == target && listed.contains_key(s.key.as_slice()))
            .collect();
        candidates.sort_by_key(|c| std::cmp::Reverse((c.updated_at, c.rowid)));
        let Some(newest) = candidates.first().copied() else {
            left.extend(leave_all(Why::NotListed));
            continue;
        };
        // A record the manifest lists under another type than its kind is the phone's
        // inconsistency: rotating it would make ours (H6).
        if candidates
            .iter()
            .any(|c| listed.get(c.key.as_slice()) != Some(&(shape.item_type() as i32)))
        {
            return Err(Refusal::TypeMismatch);
        }
        let Ok(decoded) = proto::StorageRecord::decode(&newest.raw[..]) else {
            left.extend(leave_all(Why::Unreadable));
            continue;
        };
        if oneof_type(&decoded) != Some(shape.item_type()) {
            return Err(Refusal::TypeMismatch);
        }
        let told = input.told_archived.get(target).copied();
        let rotation = match rotate(
            shape,
            target,
            &newest.raw,
            &decoded,
            &mine,
            told,
            input.pin_limit,
        ) {
            Ok(rotation) => rotation,
            Err(_) => {
                left.extend(leave_all(Why::Unreadable));
                continue;
            }
        };
        left.extend(rotation.left);
        let Some(bytes) = rotation.bytes else {
            continue;
        };
        planned.extend(rotation.written);
        inserts.push(Insert {
            key: new_id().to_vec(),
            kind: shape.item_type(),
            target: target.to_owned(),
            peer: newest.peer.clone(),
            record: Zeroizing::new(bytes),
        });
        deletes.extend(candidates.iter().map(|c| c.key.clone()));
    }
    planned.sort_by_key(|p| p.seq);

    let deleted: HashSet<&[u8]> = deletes.iter().map(Vec::as_slice).collect();
    let mut identifiers: Vec<Identifier> = fetched
        .identifiers
        .iter()
        .filter(|i| !deleted.contains(i.raw.as_slice()))
        .cloned()
        .collect();
    identifiers.extend(inserts.iter().map(|insert| Identifier {
        raw: insert.key.clone(),
        r#type: insert.kind as i32,
    }));
    let plan = Plan {
        manifest: proto::ManifestRecord {
            version: fetched.version.checked_add(1).ok_or(Refusal::Version)?,
            source_device: input.device_id,
            identifiers,
            record_ikm: fetched.record_ikm.clone(),
        },
        inserts,
        deletes,
        planned,
        left,
    };
    validate(fetched, &plan)?;
    Ok(plan)
}

// ---------------------------------------------------------------------------------------------
// Validation (H6)
// ---------------------------------------------------------------------------------------------

/// Checks `plan` against the manifest it was planned from. The rules are in the module doc; the
/// checks run in this order, so a plan that breaks several is refused for the first.
pub(crate) fn validate(
    fetched: &proto::ManifestRecord,
    plan: &Plan,
) -> std::result::Result<(), Refusal> {
    let manifest = &plan.manifest;
    if fetched.version.checked_add(1) != Some(manifest.version) {
        return Err(Refusal::Version);
    }
    if manifest.record_ikm != fetched.record_ikm {
        return Err(Refusal::RecordIkm);
    }
    let mut seen: HashSet<&[u8]> = HashSet::new();
    for id in &manifest.identifiers {
        if !seen.insert(id.raw.as_slice()) {
            return Err(Refusal::DuplicateId);
        }
    }
    if plan.deletes.iter().any(|d| seen.contains(d.as_slice())) {
        return Err(Refusal::DeleteListed);
    }
    for insert in &plan.inserts {
        if insert.key.len() != 16 {
            return Err(Refusal::BadId);
        }
        let Some(id) = manifest.identifiers.iter().find(|i| i.raw == insert.key) else {
            return Err(Refusal::InsertNotListed);
        };
        // The identifier's type, the insert's kind and the record's oneof must be one.
        let record_type = proto::StorageRecord::decode(&insert.record[..])
            .ok()
            .as_ref()
            .and_then(oneof_type);
        if record_type != Some(insert.kind) || id.r#type != insert.kind as i32 {
            return Err(Refusal::TypeMismatch);
        }
    }
    // Two account records: the phone's own inconsistency. A write that leaves them as they are
    // goes (both kept, as the duplicates of any untouched target, M7); a write that rotates the
    // account record has deleted the other candidates (M7) and lists one; a manifest that has
    // MORE account records than the fetched one, or two after the account record was touched, is
    // our bug.
    let count_accounts = |ids: &[Identifier]| {
        ids.iter()
            .filter(|i| i.r#type == ItemType::Account as i32)
            .count()
    };
    let accounts = count_accounts(&manifest.identifiers);
    if accounts > 1 {
        let touched = plan.inserts.iter().any(|i| i.kind == ItemType::Account)
            || fetched
                .identifiers
                .iter()
                .any(|i| i.r#type == ItemType::Account as i32 && plan.deletes.contains(&i.raw));
        if touched || accounts > count_accounts(&fetched.identifiers) {
            return Err(Refusal::TwoAccountRecords);
        }
    }
    // Fetched minus deleted plus inserted, nothing else: every kept identifier as the phone
    // listed it, the unknown and the failed ones included.
    let fetched_ids: HashSet<&[u8]> = fetched
        .identifiers
        .iter()
        .map(|i| i.raw.as_slice())
        .collect();
    if plan
        .deletes
        .iter()
        .any(|d| !fetched_ids.contains(d.as_slice()))
    {
        return Err(Refusal::Drift);
    }
    let deleted: HashSet<&[u8]> = plan.deletes.iter().map(Vec::as_slice).collect();
    let mut expected: HashMap<&[u8], i32> = fetched
        .identifiers
        .iter()
        .filter(|i| !deleted.contains(i.raw.as_slice()))
        .map(|i| (i.raw.as_slice(), i.r#type))
        .collect();
    for insert in &plan.inserts {
        expected.insert(insert.key.as_slice(), insert.kind as i32);
    }
    if expected.len() != manifest.identifiers.len()
        || manifest
            .identifiers
            .iter()
            .any(|i| expected.get(i.raw.as_slice()) != Some(&i.r#type))
    {
        return Err(Refusal::Drift);
    }
    Ok(())
}

mod job;
pub(crate) use job::{
    FAILPOINT_AFTER_STORAGE_WRITE_COMMIT, FAILPOINT_AFTER_STORAGE_WRITE_PUT,
    FAILPOINT_BEFORE_STORAGE_WRITE_PUT, after_landing, run, run_fetch_latest,
};

#[cfg(test)]
mod tests;
