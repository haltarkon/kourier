//! The Message Send Log and the per-recipient send progress (§5.5, §6.7).
//!
//! MSL (`SD/ts/sql/Server.node.ts` `insertSentProto`, `deleteSentProtoRecipient`,
//! `deleteSentProtosOlderThan`; `SD/ts/util/handleMessageSend.preload.ts`): every sent `Content`
//! except typing and null messages, with the devices it went to, kept 14 days so a retry request
//! can be answered with the same content. A delivery receipt removes that device's row; a
//! payload without recipients is deleted; when the receipt acknowledges a payload that carried
//! our PNI signature, the recipient has merged our number and the signature can stop.
//!
//! Send progress: the recipients the server accepted an outgoing message for, so a repeated
//! `Transmit` (after a crash between the server's answer and the core's commit, §4.8 step 4)
//! never delivers the message twice.

use std::time::Duration;

use kourier_backend_api::clock::duration_ms;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_protocol::{ContentHint, DeviceId};

use crate::error::Result;

/// How long sent payloads are kept (`desktop.retryRespondMaxAge`, default 14 days).
pub(crate) const RETENTION: Duration = Duration::from_secs(14 * 24 * 3600);
/// How long send progress is kept: longer than the core's 24 h transmit budget (§4.8).
pub(crate) const PROGRESS_RETENTION: Duration = Duration::from_secs(3 * 24 * 3600);

fn ms(d: Duration) -> i64 {
    i64::try_from(duration_ms(d)).unwrap_or(i64::MAX)
}

/// The `msl_payloads.content_hint` value.
pub(crate) fn hint_value(hint: ContentHint) -> i64 {
    i64::from(u32::from(hint))
}

/// One sent `Content` for the log.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Payload<'a> {
    /// The sent timestamp (envelope and message).
    pub(crate) timestamp: u64,
    /// The unpadded, serialized `Content`.
    pub(crate) content: &'a [u8],
    /// Its content hint.
    pub(crate) hint: ContentHint,
    /// Sent as urgent.
    pub(crate) urgent: bool,
    /// It carried our PNI signature.
    pub(crate) has_pni_signature: bool,
    /// The core's message id it belongs to, if any.
    pub(crate) local: Option<&'a str>,
    /// The group it was sent in (a resend adds our sender-key distribution message).
    pub(crate) group_id: Option<&'a [u8]>,
}

/// Records that `devices` of `recipient` (a service id string) got `payload`. A payload with the
/// same timestamp and content is shared (the same message to several recipients).
pub(crate) fn record(
    conn: &Connection,
    payload: &Payload<'_>,
    recipient: &str,
    devices: &[DeviceId],
    now: i64,
) -> Result<i64> {
    let timestamp = i64::try_from(payload.timestamp).unwrap_or(i64::MAX);
    let existing: Option<i64> = conn
        .prepare_cached("SELECT id FROM msl_payloads WHERE timestamp = ?1 AND content = ?2")?
        .query_row((timestamp, payload.content), |r| r.get(0))
        .optional()?;
    let id = match existing {
        Some(id) => id,
        None => {
            conn.prepare_cached(
                "INSERT INTO msl_payloads(timestamp, content, content_hint, urgent,
                   has_pni_signature, created_at, group_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute((
                timestamp,
                payload.content,
                hint_value(payload.hint),
                payload.urgent,
                payload.has_pni_signature,
                now,
                payload.group_id,
            ))?;
            conn.last_insert_rowid()
        }
    };
    for device in devices {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO msl_recipients(payload_id, recipient, device)
             VALUES (?1, ?2, ?3)",
        )?
        .execute((id, recipient, u32::from(*device)))?;
    }
    if let Some(local) = payload.local {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO msl_message_ids(payload_id, local) VALUES (?1, ?2)",
        )?
        .execute((id, local))?;
    }
    Ok(id)
}

/// A delivery receipt from `device` of `recipient` for the messages sent at `timestamps`:
/// removes those rows and payloads left without recipients. Returns whether it acknowledged a
/// payload with our PNI signature for every device of the recipient (Desktop's
/// `successfulPhoneNumberShares`).
pub(crate) fn delivered(
    conn: &Connection,
    recipient: &str,
    device: u32,
    timestamps: &[u64],
) -> Result<bool> {
    let mut shared_number = false;
    for timestamp in timestamps {
        let timestamp = i64::try_from(*timestamp).unwrap_or(i64::MAX);
        let payloads: Vec<(i64, bool)> = conn
            .prepare_cached(
                "SELECT p.id, p.has_pni_signature FROM msl_payloads p
                 JOIN msl_recipients r ON r.payload_id = p.id
                 WHERE p.timestamp = ?1 AND r.recipient = ?2 AND r.device = ?3",
            )?
            .query_map((timestamp, recipient, device), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (id, has_pni_signature) in payloads {
            conn.prepare_cached(
                "DELETE FROM msl_recipients WHERE payload_id = ?1 AND recipient = ?2
                   AND device = ?3",
            )?
            .execute((id, recipient, device))?;
            let left_for_recipient: i64 = conn
                .prepare_cached(
                    "SELECT count(*) FROM msl_recipients WHERE payload_id = ?1 AND recipient = ?2",
                )?
                .query_row((id, recipient), |r| r.get(0))?;
            if has_pni_signature && left_for_recipient == 0 {
                shared_number = true;
            }
            conn.prepare_cached(
                "DELETE FROM msl_payloads WHERE id = ?1
                   AND NOT EXISTS (SELECT 1 FROM msl_recipients WHERE payload_id = ?1)",
            )?
            .execute([id])?;
        }
    }
    Ok(shared_number)
}

/// A payload of the log, as a retry request is answered with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Logged {
    /// The unpadded, serialized `Content` (not log-safe).
    pub(crate) content: Vec<u8>,
    /// Its content hint (`msl_payloads.content_hint`).
    pub(crate) hint: i64,
    /// Sent as urgent.
    pub(crate) urgent: bool,
    /// The group it was sent in.
    pub(crate) group_id: Option<Vec<u8>>,
}

/// A logged payload `device` of `recipient` got at `timestamp`, for answering a retry request.
pub(crate) fn find(
    conn: &Connection,
    recipient: &str,
    device: u32,
    timestamp: u64,
) -> Result<Option<Logged>> {
    Ok(conn
        .prepare_cached(
            "SELECT p.content, p.content_hint, p.urgent, p.group_id FROM msl_payloads p
             JOIN msl_recipients r ON r.payload_id = p.id
             WHERE p.timestamp = ?1 AND r.recipient = ?2 AND r.device = ?3
             ORDER BY p.id DESC LIMIT 1",
        )?
        .query_row(
            (
                i64::try_from(timestamp).unwrap_or(i64::MAX),
                recipient,
                device,
            ),
            |r| {
                Ok(Logged {
                    content: r.get(0)?,
                    hint: r.get(1)?,
                    urgent: r.get(2)?,
                    group_id: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Deletes payloads older than [`RETENTION`] and send progress older than
/// [`PROGRESS_RETENTION`]. Returns the number of payloads deleted.
pub(crate) fn purge(conn: &Connection, now: i64) -> Result<usize> {
    let payloads = conn
        .prepare_cached("DELETE FROM msl_payloads WHERE created_at < ?1")?
        .execute([now.saturating_sub(ms(RETENTION))])?;
    conn.prepare_cached("DELETE FROM send_progress WHERE accepted_at < ?1")?
        .execute([now.saturating_sub(ms(PROGRESS_RETENTION))])?;
    Ok(payloads)
}

/// Whether the server already accepted message `local` for `recipient`; `Some(sealed)` if so.
pub(crate) fn accepted(conn: &Connection, local: &str, recipient: &str) -> Result<Option<bool>> {
    Ok(conn
        .prepare_cached("SELECT sealed FROM send_progress WHERE local = ?1 AND recipient = ?2")?
        .query_row((local, recipient), |r| r.get(0))
        .optional()?)
}

/// Records that the server accepted message `local` for `recipient`.
pub(crate) fn record_accepted(
    conn: &Connection,
    local: &str,
    recipient: &str,
    sealed: bool,
    now: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO send_progress(local, recipient, sealed, accepted_at)
         VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute((local, recipient, sealed, now))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, kourier_db::ZoneDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([9; 32]),
        )
        .unwrap();
        (dir, db)
    }

    fn device(n: u32) -> DeviceId {
        DeviceId::new(u8::try_from(n).unwrap()).unwrap()
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn delivery_receipts_shrink_the_log_and_ack_the_pni_signature() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            let message = Payload {
                timestamp: 100,
                content: b"content",
                hint: ContentHint::Resendable,
                urgent: true,
                has_pni_signature: true,
                local: Some("100"),
                group_id: None,
            };
            let a = record(tx, &message, "alice", &[device(1), device(2)], 10)?;
            // The same content to a second recipient shares the payload.
            assert_eq!(record(tx, &message, "bob", &[device(1)], 10)?, a);
            assert_eq!(count(tx, "msl_payloads"), 1);
            assert_eq!(count(tx, "msl_recipients"), 3);
            assert_eq!(
                find(tx, "alice", 2, 100)?,
                Some(Logged {
                    content: b"content".to_vec(),
                    hint: 1,
                    urgent: true,
                    group_id: None,
                })
            );
            assert!(find(tx, "alice", 3, 100)?.is_none());

            assert!(
                !delivered(tx, "alice", 1, &[100])?,
                "alice device 2 has not"
            );
            assert!(
                delivered(tx, "alice", 2, &[100])?,
                "every device of alice did"
            );
            assert!(
                !delivered(tx, "alice", 2, &[100])?,
                "a repeated receipt is a no-op"
            );
            assert_eq!(count(tx, "msl_payloads"), 1, "bob still owes a receipt");
            assert!(delivered(tx, "bob", 1, &[100, 999])?);
            assert_eq!(count(tx, "msl_payloads"), 0, "no recipient left");
            assert_eq!(count(tx, "msl_message_ids"), 0, "cascaded");

            // Another content with the same timestamp is its own payload.
            record(
                tx,
                &Payload {
                    content: b"sync",
                    has_pni_signature: false,
                    ..message
                },
                "own",
                &[device(1)],
                10,
            )?;
            assert!(!delivered(tx, "own", 1, &[100])?, "no PNI signature in it");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn purge_keeps_fourteen_days_and_progress_three() {
        let (_dir, db) = db();
        let day = 24 * 3600 * 1000;
        db.zone(|tx| -> Result<()> {
            let payload = Payload {
                timestamp: 1,
                content: b"old",
                hint: ContentHint::Default,
                urgent: false,
                has_pni_signature: false,
                local: None,
                group_id: None,
            };
            record(tx, &payload, "x", &[device(1)], 0)?;
            record(
                tx,
                &Payload {
                    content: b"new",
                    ..payload
                },
                "x",
                &[device(1)],
                10 * day,
            )?;
            record_accepted(tx, "1", "x", true, 0)?;
            record_accepted(tx, "2", "x", false, 12 * day)?;
            assert_eq!(accepted(tx, "1", "x")?, Some(true));
            assert_eq!(accepted(tx, "2", "x")?, Some(false));
            assert_eq!(accepted(tx, "2", "y")?, None);
            assert_eq!(purge(tx, 14 * day + 1)?, 1);
            assert_eq!(count(tx, "msl_payloads"), 1);
            assert_eq!(accepted(tx, "1", "x")?, None, "progress older than 3 days");
            assert_eq!(accepted(tx, "2", "x")?, Some(false));
            Ok(())
        })
        .unwrap();
    }
}
