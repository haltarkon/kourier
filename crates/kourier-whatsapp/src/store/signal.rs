//! `SignalStore`: identities, sessions, prekeys, signed prekeys and sender keys. Every batch method
//! is overridden as ONE job and ONE transaction, so a batch is atomic and a decrypt burst costs one
//! fsync (`synchronous = FULL`), not one per row.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use kourier_db::rusqlite::params;
use wacore::store::error::Result;
use wacore::store::traits::SignalStore;

use super::{KourierStore, blob, exec, first, rows};
use crate::db::Points;
use crate::error::invalid;
use crate::failpoints;

/// The kill points of every batch of Signal state.
const BATCH: Points = Points {
    inside: failpoints::INSIDE_SESSIONS_BATCH,
    after: failpoints::AFTER_SESSIONS_BATCH,
};

/// The id of a prekey as stored (a `u32` in an INTEGER column).
fn key_id(id: u32) -> i64 {
    i64::from(id)
}

#[async_trait]
impl SignalStore for KourierStore {
    // --- Identities ---

    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()> {
        const OP: &str = "put identity failed";
        let address = address.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO identities(address, key) VALUES (?1, ?2) \
                 ON CONFLICT(address) DO UPDATE SET key = excluded.key",
                params![address, key.as_slice()],
            )?;
            Ok(())
        })
        .await
    }

    async fn put_identities_batch(&self, identities: &[(Arc<str>, [u8; 32])]) -> Result<()> {
        const OP: &str = "put identities failed";
        let identities = identities.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for (address, key) in &identities {
                exec(
                    tx,
                    OP,
                    "INSERT INTO identities(address, key) VALUES (?1, ?2) \
                     ON CONFLICT(address) DO UPDATE SET key = excluded.key",
                    params![&**address, key.as_slice()],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn load_identity(&self, address: &str) -> Result<Option<[u8; 32]>> {
        const OP: &str = "load identity failed";
        let address = address.to_owned();
        self.read(move |conn| {
            let key = first(
                conn,
                OP,
                "SELECT key FROM identities WHERE address = ?1",
                params![address],
                |row| blob(row, 0),
            )?;
            key.map(|k| {
                <[u8; 32]>::try_from(k.as_slice())
                    .map_err(|_| invalid("a stored identity key is not 32 bytes"))
            })
            .transpose()
        })
        .await
    }

    async fn delete_identity(&self, address: &str) -> Result<()> {
        const OP: &str = "delete identity failed";
        let address = address.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM identities WHERE address = ?1",
                params![address],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_identities_batch(&self, addresses: &[Arc<str>]) -> Result<()> {
        const OP: &str = "delete identities failed";
        let addresses = addresses.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for address in &addresses {
                exec(
                    tx,
                    OP,
                    "DELETE FROM identities WHERE address = ?1",
                    params![&**address],
                )?;
            }
            Ok(())
        })
        .await
    }

    // --- Sessions ---

    async fn get_session(&self, address: &str) -> Result<Option<Bytes>> {
        const OP: &str = "get session failed";
        let address = address.to_owned();
        self.read(move |conn| {
            Ok(first(
                conn,
                OP,
                "SELECT record FROM sessions WHERE address = ?1",
                params![address],
                |row| blob(row, 0),
            )?
            .map(Bytes::from))
        })
        .await
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()> {
        const OP: &str = "put session failed";
        let address = address.to_owned();
        let session = session.to_vec();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO sessions(address, record) VALUES (?1, ?2) \
                 ON CONFLICT(address) DO UPDATE SET record = excluded.record",
                params![address, session],
            )?;
            Ok(())
        })
        .await
    }

    async fn put_sessions_batch(&self, sessions: &[(Arc<str>, Bytes)]) -> Result<()> {
        const OP: &str = "put sessions failed";
        let sessions = sessions.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for (address, session) in &sessions {
                exec(
                    tx,
                    OP,
                    "INSERT INTO sessions(address, record) VALUES (?1, ?2) \
                     ON CONFLICT(address) DO UPDATE SET record = excluded.record",
                    params![&**address, session.as_ref()],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn get_sessions_batch(&self, addresses: &[Arc<str>]) -> Result<Vec<(Arc<str>, Bytes)>> {
        const OP: &str = "get sessions failed";
        let addresses = addresses.to_vec();
        self.read(move |conn| {
            let mut found = Vec::with_capacity(addresses.len());
            for address in &addresses {
                let record = first(
                    conn,
                    OP,
                    "SELECT record FROM sessions WHERE address = ?1",
                    params![&**address],
                    |row| blob(row, 0),
                )?;
                if let Some(record) = record {
                    found.push((Arc::clone(address), Bytes::from(record)));
                }
            }
            Ok(found)
        })
        .await
    }

    async fn delete_session(&self, address: &str) -> Result<()> {
        const OP: &str = "delete session failed";
        let address = address.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM sessions WHERE address = ?1",
                params![address],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_sessions_batch(&self, addresses: &[Arc<str>]) -> Result<()> {
        const OP: &str = "delete sessions failed";
        let addresses = addresses.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for address in &addresses {
                exec(
                    tx,
                    OP,
                    "DELETE FROM sessions WHERE address = ?1",
                    params![&**address],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn has_session(&self, address: &str) -> Result<bool> {
        const OP: &str = "has session failed";
        let address = address.to_owned();
        self.read(move |conn| {
            Ok(first(
                conn,
                OP,
                "SELECT 1 FROM sessions WHERE address = ?1",
                params![address],
                |row| row.get::<_, i64>(0),
            )?
            .is_some())
        })
        .await
    }

    /// Whether any session or identity exists for `user` across all device ids: an address is
    /// `user@server` or `user:device@server`, so the rows are the two key ranges `user@…` and
    /// `user:…`. Ranges, not `LIKE`: SQLite uses a `LIKE` prefix on an index only for NOCASE
    /// columns, and these keys are BINARY. A range ends at the prefix with its last byte raised by
    /// one (`@` to `A`, `:` to `;`), which is above every string that starts with the prefix.
    async fn has_signal_state_for_user(&self, user: &str) -> Result<bool> {
        const OP: &str = "has signal state failed";
        let ranges = [
            (format!("{user}@"), format!("{user}A")),
            (format!("{user}:"), format!("{user};")),
        ];
        self.read(move |conn| {
            for table_sql in HAS_STATE_SQL {
                for (from, to) in &ranges {
                    let found = first(conn, OP, table_sql, params![from, to], |row| {
                        row.get::<_, i64>(0)
                    })?;
                    if found.is_some() {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        })
        .await
    }

    // --- Prekeys ---

    async fn store_prekey(&self, id: u32, record: &[u8], uploaded: bool) -> Result<()> {
        const OP: &str = "store prekey failed";
        let record = record.to_vec();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                STORE_PREKEY,
                params![key_id(id), record, i64::from(uploaded)],
            )?;
            Ok(())
        })
        .await
    }

    async fn store_prekeys_batch(&self, keys: &[(u32, Bytes)], uploaded: bool) -> Result<()> {
        const OP: &str = "store prekeys failed";
        let keys = keys.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for (id, record) in &keys {
                exec(
                    tx,
                    OP,
                    STORE_PREKEY,
                    params![key_id(*id), record.as_ref(), i64::from(uploaded)],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn load_prekey(&self, id: u32) -> Result<Option<Bytes>> {
        const OP: &str = "load prekey failed";
        self.read(move |conn| {
            Ok(first(
                conn,
                OP,
                "SELECT record FROM prekeys WHERE id = ?1",
                params![key_id(id)],
                |row| blob(row, 0),
            )?
            .map(Bytes::from))
        })
        .await
    }

    async fn load_prekeys_batch(&self, ids: &[u32]) -> Result<Vec<(u32, Bytes)>> {
        const OP: &str = "load prekeys failed";
        let ids = ids.to_vec();
        self.read(move |conn| {
            let mut found = Vec::with_capacity(ids.len());
            for id in ids {
                let record = first(
                    conn,
                    OP,
                    "SELECT record FROM prekeys WHERE id = ?1",
                    params![key_id(id)],
                    |row| blob(row, 0),
                )?;
                if let Some(record) = record {
                    found.push((id, Bytes::from(record)));
                }
            }
            Ok(found)
        })
        .await
    }

    /// UPDATE semantics on purpose: a prekey consumed (deleted) between the upload snapshot and
    /// this call stays deleted, never resurrected by an upsert.
    async fn mark_prekeys_uploaded(&self, ids: &[u32]) -> Result<()> {
        const OP: &str = "mark prekeys uploaded failed";
        let ids = ids.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for id in &ids {
                exec(
                    tx,
                    OP,
                    "UPDATE prekeys SET uploaded = 1 WHERE id = ?1",
                    params![key_id(*id)],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn remove_prekey(&self, id: u32) -> Result<()> {
        const OP: &str = "remove prekey failed";
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM prekeys WHERE id = ?1",
                params![key_id(id)],
            )?;
            Ok(())
        })
        .await
    }

    async fn remove_prekeys_batch(&self, ids: &[u32]) -> Result<()> {
        const OP: &str = "remove prekeys failed";
        let ids = ids.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for id in &ids {
                exec(
                    tx,
                    OP,
                    "DELETE FROM prekeys WHERE id = ?1",
                    params![key_id(*id)],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn get_max_prekey_id(&self) -> Result<u32> {
        const OP: &str = "get max prekey id failed";
        self.read(move |conn| {
            let max = first(
                conn,
                OP,
                "SELECT COALESCE(MAX(id), 0) FROM prekeys",
                [],
                |row| row.get::<_, i64>(0),
            )?
            .unwrap_or(0);
            u32::try_from(max).map_err(|_| invalid("a stored prekey id is out of range"))
        })
        .await
    }

    // --- Signed prekeys ---

    async fn store_signed_prekey(&self, id: u32, record: &[u8]) -> Result<()> {
        const OP: &str = "store signed prekey failed";
        let record = record.to_vec();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO signed_prekeys(id, record) VALUES (?1, ?2) \
                 ON CONFLICT(id) DO UPDATE SET record = excluded.record",
                params![key_id(id), record],
            )?;
            Ok(())
        })
        .await
    }

    async fn load_signed_prekey(&self, id: u32) -> Result<Option<Vec<u8>>> {
        const OP: &str = "load signed prekey failed";
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT record FROM signed_prekeys WHERE id = ?1",
                params![key_id(id)],
                |row| blob(row, 0),
            )
        })
        .await
    }

    async fn load_all_signed_prekeys(&self) -> Result<Vec<(u32, Vec<u8>)>> {
        const OP: &str = "load signed prekeys failed";
        self.read(move |conn| {
            let all = rows(
                conn,
                OP,
                "SELECT id, record FROM signed_prekeys ORDER BY id",
                [],
                |row| Ok((row.get::<_, i64>(0)?, blob(row, 1)?)),
            )?;
            all.into_iter()
                .map(|(id, record)| {
                    u32::try_from(id)
                        .map(|id| (id, record))
                        .map_err(|_| invalid("a stored signed prekey id is out of range"))
                })
                .collect()
        })
        .await
    }

    async fn remove_signed_prekey(&self, id: u32) -> Result<()> {
        const OP: &str = "remove signed prekey failed";
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM signed_prekeys WHERE id = ?1",
                params![key_id(id)],
            )?;
            Ok(())
        })
        .await
    }

    // --- Sender keys ---

    async fn put_sender_key(&self, address: &str, record: &[u8]) -> Result<()> {
        const OP: &str = "put sender key failed";
        let address = address.to_owned();
        let record = record.to_vec();
        self.write(OP, move |tx| {
            exec(tx, OP, PUT_SENDER_KEY, params![address, record])?;
            Ok(())
        })
        .await
    }

    async fn put_sender_keys_batch(&self, sender_keys: &[(Arc<str>, Bytes)]) -> Result<()> {
        const OP: &str = "put sender keys failed";
        let sender_keys = sender_keys.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for (address, record) in &sender_keys {
                exec(tx, OP, PUT_SENDER_KEY, params![&**address, record.as_ref()])?;
            }
            Ok(())
        })
        .await
    }

    async fn get_sender_key(&self, address: &str) -> Result<Option<Vec<u8>>> {
        const OP: &str = "get sender key failed";
        let address = address.to_owned();
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT record FROM sender_keys WHERE address = ?1",
                params![address],
                |row| blob(row, 0),
            )
        })
        .await
    }

    async fn delete_sender_key(&self, address: &str) -> Result<()> {
        const OP: &str = "delete sender key failed";
        let address = address.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM sender_keys WHERE address = ?1",
                params![address],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_sender_keys_batch(&self, addresses: &[Arc<str>]) -> Result<()> {
        const OP: &str = "delete sender keys failed";
        let addresses = addresses.to_vec();
        self.write_at(OP, BATCH, move |tx| {
            for address in &addresses {
                exec(
                    tx,
                    OP,
                    "DELETE FROM sender_keys WHERE address = ?1",
                    params![&**address],
                )?;
            }
            Ok(())
        })
        .await
    }
}

const STORE_PREKEY: &str = "INSERT INTO prekeys(id, record, uploaded) VALUES (?1, ?2, ?3) \
     ON CONFLICT(id) DO UPDATE SET record = excluded.record, uploaded = excluded.uploaded";

const PUT_SENDER_KEY: &str = "INSERT INTO sender_keys(address, record) VALUES (?1, ?2) \
     ON CONFLICT(address) DO UPDATE SET record = excluded.record";

/// The two lookups of `has_signal_state_for_user`, one per table, each a key range of its primary
/// key (`?1 <= address < ?2`). They are statements of their own so a plan test can look at both.
pub(crate) const HAS_STATE_SQL: [&str; 2] = [
    "SELECT 1 FROM sessions WHERE address >= ?1 AND address < ?2 LIMIT 1",
    "SELECT 1 FROM identities WHERE address >= ?1 AND address < ?2 LIMIT 1",
];
