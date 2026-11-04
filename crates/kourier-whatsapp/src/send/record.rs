//! The upload record (W5b, decision G4; brief §2.4): what WhatsApp's media server answered for the file of one own
//! message, with the media key it was encrypted under, kept in `backend.db` (`media_uploads`, migration 6).
//!
//! ORDER. The record is committed in ONE zone ([`commit`], kill points `wa_inside_media_upload` /
//! `wa_after_media_upload`) after the upload returned and BEFORE the message is built and its frame written; the
//! attempt's one store read reads it ([`key_of`], [`look`]). A kill (or any transient answer) after the commit and before
//! the ledger's: the core's resend of the same local sends the SAME file from the record (same key, path and
//! hashes) with no second upload, so a second frame of the local (D21's allowed one) carries identical content.
//!
//! HOW LONG. A resend reuses the record only within [`REUSE_MS`] (6 hours) of the upload ([`Stored::reusable`]):
//! whether the media server keeps an upload no message names any longer is unknown (U3, the real run R3 looks); a
//! later resend uploads again and the zone replaces the row. The row itself is kept [`KEEP_MS`] (30 days) and swept
//! at a start ([`sweep`]): the media key and the pointer of an own file are what the caption edit reads and what a
//! later re-upload request (W7) needs; WhatsApp's media live about that long.
//!
//! WHEN IT GOES BEFORE THAT (fix F13). A message deleted for everyone: the acked revoke's own zone deletes its row
//! (`ledger::record`, [`forget`]); the caption edit and a re-upload request have nothing left to write. A message that
//! ends stopped or refused for good after its record was committed: the attempt deletes its own row ([`discard`]; a
//! failed delete is logged by kind and left to the sweep). The chat clear (`ClearConversation`, W6a1 D14) deletes
//! `WHERE conv = ?` ([`forget_chat`]) in the zone that confirms the command. The PHONE's clear or delete of a chat does the
//! same in the zone that maps it, except that a clear by a range time keeps the records after the range
//! ([`forget_chat_until`], G4).
//!
//! The media key is a secret inside SQLCipher like every other key of `backend.db`; nothing of a row is logged (the
//! pointer's `Debug` prints its type and length only).

use kourier_backend_api::{OutgoingKind, OutgoingRequest};
use kourier_db::rusqlite::{Connection, Transaction, params};
use kourier_model::{Body, ConvKey};
use wacore::store::error::StoreError;

use crate::db::Points;
use crate::media::MediaPointer;
use crate::store::{KourierStore, exec, first};

/// The kill points of the record's zone.
pub(crate) const POINTS: Points = Points {
    inside: crate::failpoints::INSIDE_MEDIA_UPLOAD,
    after: crate::failpoints::AFTER_MEDIA_UPLOAD,
};

/// How long a row is kept: 30 days (G4).
pub(crate) const KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// How long after its upload a record is sent again by a resend instead of a new upload: 6 hours (G4, U3).
pub(crate) const REUSE_MS: i64 = 6 * 60 * 60 * 1000;

/// A record as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stored {
    /// The uploaded file: what the message is built from.
    pub(crate) pointer: MediaPointer,
    /// When the upload was answered (wall clock, ms).
    pub(crate) at_ms: i64,
}

impl Stored {
    /// Whether a resend at `now_ms` sends this record again instead of uploading anew: younger than [`REUSE_MS`].
    /// A record from the future (a clock set back since) is not judged young: the file is uploaded again, which
    /// costs bytes, never a wrong message.
    pub(crate) fn reusable(&self, now_ms: i64) -> bool {
        let age = now_ms.saturating_sub(self.at_ms);
        (0..REUSE_MS).contains(&age)
    }
}

/// The record of the message `local`, if any. A row whose pointer does not read back is no record (the file is
/// uploaded again; logged without an id).
pub(crate) fn look(conn: &Connection, local: &str) -> Result<Option<Stored>, StoreError> {
    let row = first(
        conn,
        "read an upload record",
        "SELECT pointer, at FROM media_uploads WHERE local = ?1",
        params![local],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
    )?;
    Ok(
        row.and_then(|(pointer, at_ms)| match serde_json::from_slice(&pointer) {
            Ok(pointer) => Some(Stored { pointer, at_ms }),
            Err(_) => {
                tracing::warn!("an upload record did not read back; the file is uploaded again");
                None
            }
        }),
    )
}

/// Whose record the attempt of `req` reads in its one store read: a new message with a file reads its OWN (a resend
/// of it), a caption edit reads its TARGET's (the edit of an own file writes the file the record names); every other
/// request reads none.
pub(crate) fn key_of(req: &OutgoingRequest) -> Option<String> {
    match &req.kind {
        OutgoingKind::Message {
            body: Body::Media { .. },
            ..
        } => Some(req.local.to_string()),
        OutgoingKind::Edit {
            target,
            body: Body::Media { .. },
        } => Some(target.local.to_string()),
        _ => None,
    }
}

/// Writes the record of `local` in the caller's zone, replacing an older one of the same local (a resend after
/// [`REUSE_MS`] uploaded again).
pub(crate) fn put(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    local: &str,
    pointer: &MediaPointer,
    at_ms: i64,
) -> Result<(), StoreError> {
    // Strings and numbers only: the encoding cannot fail.
    let json = serde_json::to_vec(pointer).unwrap_or_default();
    exec(
        tx,
        "write an upload record",
        "INSERT INTO media_uploads(local, conv, pointer, at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(local) DO UPDATE SET conv = excluded.conv, pointer = excluded.pointer, at = excluded.at",
        params![local, conv.encode(), json, at_ms],
    )
    .map(drop)
}

/// [`put`] in its own zone with the kill points, on the store's writer: `Ok` only after `COMMIT`.
pub(crate) async fn commit(
    store: &KourierStore,
    conv: ConvKey,
    local: String,
    pointer: MediaPointer,
    at_ms: i64,
) -> Result<(), StoreError> {
    store
        .write_at("record an upload", POINTS, move |tx| {
            put(tx, &conv, &local, &pointer, at_ms)
        })
        .await
}

/// Forgets the record of `local` in the caller's zone (fix F13: a message deleted for everyone, or one that will
/// never be sent, has no use for the media key, the paths and the preview of its file). Returns how many rows went.
pub(crate) fn forget(tx: &Transaction<'_>, local: &str) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget an upload record",
        "DELETE FROM media_uploads WHERE local = ?1",
        params![local],
    )
}

/// [`forget`] in its own zone, on the store's writer: `Ok` only after `COMMIT`.
pub(crate) async fn discard(store: &KourierStore, local: String) -> Result<(), StoreError> {
    store
        .write("forget an upload record", move |tx| {
            forget(tx, &local).map(drop)
        })
        .await
}

/// Forgets every record of the chat `conv` in the caller's zone (W6a1 D14: the chat was cleared or deleted here, so
/// no re-upload request can come for its files, and the records hold their media keys). Returns how many rows went.
pub(crate) fn forget_chat(tx: &Transaction<'_>, conv: &ConvKey) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget the upload records of a chat",
        "DELETE FROM media_uploads WHERE conv = ?1",
        params![conv.encode()],
    )
}

/// Forgets the records of the chat `conv` that were uploaded at or before `until_ms` (the same zone rules as
/// [`forget_chat`]): the phone's clear by a RANGE removes the messages up to the range's end, and a file Kourier sent
/// after it is still in the chat, so its record (the media key a caption edit and a re-upload request read) stays (G4).
/// The bound is the range's time in the phone's clock against the upload's in ours: a skew can only keep a record whose
/// message was cleared, which the 30-day sweep takes. Returns how many rows went.
pub(crate) fn forget_chat_until(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    until_ms: i64,
) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget the upload records of a cleared range",
        "DELETE FROM media_uploads WHERE conv = ?1 AND at <= ?2",
        params![conv.encode(), until_ms],
    )
}

/// Forgets the rows older than [`KEEP_MS`] before `now_ms`, in the caller's zone. Returns how many went.
pub(crate) fn sweep(tx: &Transaction<'_>, now_ms: i64) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget old upload records",
        "DELETE FROM media_uploads WHERE at < ?1",
        [now_ms.saturating_sub(KEEP_MS)],
    )
}

#[cfg(test)]
mod tests;
