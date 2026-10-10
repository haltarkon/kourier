//! `SenderKeyStore` over the `sender_keys` table: `(sender address, distribution id) →
//! SenderKeyRecord` (`SD/ts/SignalProtocolStore.preload.ts` `saveSenderKey`/`getSenderKey`).
//! Only other members' chains live here in S10 (receive); our own chains arrive with group send
//! in S11.

use async_trait::async_trait;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_protocol::{ProtocolAddress, SenderKeyRecord, SenderKeyStore, SignalProtocolError};
use uuid::Uuid;

use super::{device_of, sql_error};

/// Sender keys on one zone's connection.
pub(crate) struct SqlSenderKeyStore<'c> {
    conn: &'c Connection,
    now: i64,
}

impl<'c> SqlSenderKeyStore<'c> {
    /// The store on `conn`; `now` (ms) stamps the rows it writes.
    pub(crate) fn new(conn: &'c Connection, now: i64) -> Self {
        Self { conn, now }
    }
}

#[async_trait(?Send)]
impl SenderKeyStore for SqlSenderKeyStore<'_> {
    async fn store_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
        record: &SenderKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        let bytes = record.serialize()?;
        self.conn
            .prepare_cached(
                "INSERT INTO sender_keys(sender, device, distribution_id, record, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(sender, device, distribution_id)
                 DO UPDATE SET record = excluded.record, updated_at = excluded.updated_at",
            )
            .and_then(|mut stmt| {
                stmt.execute((
                    sender.name(),
                    device_of(sender),
                    &distribution_id.as_bytes()[..],
                    &bytes,
                    self.now,
                ))
            })
            .map(drop)
            .map_err(|e| sql_error("store sender key", &e))
    }

    async fn load_sender_key(
        &mut self,
        sender: &ProtocolAddress,
        distribution_id: Uuid,
    ) -> Result<Option<SenderKeyRecord>, SignalProtocolError> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .prepare_cached(
                "SELECT record FROM sender_keys
                 WHERE sender = ?1 AND device = ?2 AND distribution_id = ?3",
            )
            .and_then(|mut stmt| {
                stmt.query_row(
                    (
                        sender.name(),
                        device_of(sender),
                        &distribution_id.as_bytes()[..],
                    ),
                    |r| r.get(0),
                )
                .optional()
            })
            .map_err(|e| sql_error("load sender key", &e))?;
        bytes
            .map(|bytes| SenderKeyRecord::deserialize(&bytes))
            .transpose()
    }
}
