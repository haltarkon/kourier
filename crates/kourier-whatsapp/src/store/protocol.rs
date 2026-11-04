//! `ProtocolStore` except the pending-inbound buffer (`pending.rs`): sender-key device tracking, the
//! LID to phone-number mapping, base keys, the device registry, group metadata, trusted-contact
//! tokens and the sent-message store.
//!
//! The two token merges (`touch_tc_token_sender_timestamp`, `store_received_tc_token`) are ONE
//! statement each, an upsert whose conflict branch holds the rule; with one connection on one thread
//! that is the "atomic merge serialized against every writer of the row" the trait asks for, and
//! a read followed by a write (two jobs) is exactly what it forbids.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use kourier_db::rusqlite::params;
use wacore::store::error::Result;
use wacore::store::traits::{
    DeviceListRecord, LidPnMappingEntry, PendingInboundKey, PendingInboundLease,
    PendingInboundMutation, PendingInboundPayload, PendingInboundRecord, PendingInboundRevision,
    PendingInboundRow, ProtocolStore, TcTokenEntry,
};

use super::{KourierStore, blob, exec, first, rows};
use crate::error::unreadable;

/// `touch_tc_token_sender_timestamp`'s merge, ONE statement (its doc): also run by the history's delivery zone (W5a,
/// D12), so both writers of a history token apply the same rule.
const TOUCH_TC_SENDER_TIMESTAMP: &str = "INSERT INTO tc_tokens(jid, token, token_timestamp, sender_timestamp) \
     VALUES (?1, x'', ?2, ?2) \
     ON CONFLICT(jid) DO UPDATE SET sender_timestamp = \
       max(coalesce(sender_timestamp, excluded.sender_timestamp), excluded.sender_timestamp)";

/// `store_received_tc_token`'s newer-wins merge, ONE statement (its doc); shared as above.
const STORE_RECEIVED_TC_TOKEN: &str = "INSERT INTO tc_tokens(jid, token, token_timestamp, sender_timestamp) \
     VALUES (?1, ?2, ?3, NULL) \
     ON CONFLICT(jid) DO UPDATE SET token = excluded.token, \
       token_timestamp = excluded.token_timestamp \
     WHERE length(tc_tokens.token) = 0 \
       OR excluded.token_timestamp >= tc_tokens.token_timestamp";

/// A history chunk's token in the CALLER's zone (W5a, D12: W4's delivery zone, the kill-safe copy of what upstream
/// stores only after the chunk's receipt), as upstream's `store_tc_token_candidate` does it: the newer-wins merge
/// under `key` (the LID user when known, else the number's user), then the sender time when the chunk states one.
pub(crate) fn keep_history_token(
    tx: &kourier_db::rusqlite::Transaction<'_>,
    key: &str,
    token: &[u8],
    at: i64,
    sender_at: Option<i64>,
) -> Result<()> {
    exec(
        tx,
        "store a history tc token failed",
        STORE_RECEIVED_TC_TOKEN,
        params![key, token, at],
    )?;
    if let Some(sender_at) = sender_at {
        exec(
            tx,
            "touch a history tc token failed",
            TOUCH_TC_SENDER_TIMESTAMP,
            params![key, sender_at],
        )?;
    }
    Ok(())
}

/// Unix seconds now (the store has no injected clock; the rows it stamps are only ever compared
/// with cutoffs the client computes from its own clock).
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

const TC_COLUMNS: &str = "token, token_timestamp, sender_timestamp";

fn tc_entry(row: &kourier_db::rusqlite::Row<'_>) -> kourier_db::rusqlite::Result<TcTokenEntry> {
    Ok(TcTokenEntry {
        token: row.get(0)?,
        token_timestamp: row.get(1)?,
        sender_timestamp: row.get(2)?,
    })
}

fn lid_entry(
    row: &kourier_db::rusqlite::Row<'_>,
) -> kourier_db::rusqlite::Result<LidPnMappingEntry> {
    Ok(LidPnMappingEntry {
        lid: row.get(0)?,
        phone_number: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        learning_source: row.get(4)?,
    })
}

const LID_COLUMNS: &str = "lid, phone, created_at, updated_at, learning_source";

const PUT_LID: &str = "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
     VALUES (?1, ?2, ?3, ?4, ?5) \
     ON CONFLICT(lid) DO UPDATE SET phone = excluded.phone, created_at = excluded.created_at, \
     updated_at = excluded.updated_at, learning_source = excluded.learning_source";

const PUT_DEVICES: &str = "INSERT INTO device_registry(user, record) VALUES (?1, ?2) \
     ON CONFLICT(user) DO UPDATE SET record = excluded.record";

/// The record of a device list: the JSON upstream persists.
fn encode_devices(record: &DeviceListRecord) -> Result<Vec<u8>> {
    serde_json::to_vec(record).map_err(|_| unreadable("a device list could not be serialized"))
}

fn decode_devices(record: &[u8]) -> Result<DeviceListRecord> {
    serde_json::from_slice(record).map_err(|_| unreadable("a stored device list could not be read"))
}

#[async_trait]
impl ProtocolStore for KourierStore {
    // --- Per-device sender key tracking ---

    async fn get_sender_key_devices(&self, group_jid: &str) -> Result<Vec<(String, bool)>> {
        const OP: &str = "get sender key devices failed";
        let group_jid = group_jid.to_owned();
        self.read(move |conn| {
            rows(
                conn,
                OP,
                "SELECT device_jid, has_key FROM sender_key_devices WHERE group_jid = ?1 \
                 ORDER BY device_jid",
                params![group_jid],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
            )
        })
        .await
    }

    async fn set_sender_key_status(&self, group_jid: &str, entries: &[(&str, bool)]) -> Result<()> {
        const OP: &str = "set sender key status failed";
        let group_jid = group_jid.to_owned();
        let entries: Vec<(String, bool)> = entries
            .iter()
            .map(|(jid, has)| ((*jid).to_owned(), *has))
            .collect();
        self.write(OP, move |tx| {
            for (device_jid, has_key) in &entries {
                exec(
                    tx,
                    OP,
                    "INSERT INTO sender_key_devices(group_jid, device_jid, has_key) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(group_jid, device_jid) DO UPDATE SET has_key = excluded.has_key",
                    params![group_jid, device_jid, i64::from(*has_key)],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn clear_sender_key_devices(&self, group_jid: &str) -> Result<()> {
        const OP: &str = "clear sender key devices failed";
        let group_jid = group_jid.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM sender_key_devices WHERE group_jid = ?1",
                params![group_jid],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_sender_key_device_rows(&self, device_jids: &[&str]) -> Result<()> {
        const OP: &str = "delete sender key device rows failed";
        let device_jids: Vec<String> = device_jids.iter().map(|jid| (*jid).to_owned()).collect();
        self.write(OP, move |tx| {
            for device_jid in &device_jids {
                exec(
                    tx,
                    OP,
                    "DELETE FROM sender_key_devices WHERE device_jid = ?1",
                    params![device_jid],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn clear_all_sender_key_devices(&self) -> Result<()> {
        const OP: &str = "clear all sender key devices failed";
        self.write(OP, move |tx| {
            exec(tx, OP, "DELETE FROM sender_key_devices", [])?;
            Ok(())
        })
        .await
    }

    // --- LID to phone-number mapping ---

    async fn get_lid_mapping(&self, lid: &str) -> Result<Option<LidPnMappingEntry>> {
        const OP: &str = "get lid mapping failed";
        let lid = lid.to_owned();
        self.read(move |conn| {
            first(
                conn,
                OP,
                &format!("SELECT {LID_COLUMNS} FROM lid_pn_mapping WHERE lid = ?1"),
                params![lid],
                lid_entry,
            )
        })
        .await
    }

    /// A phone number can have several LIDs (it gets a new one; the old LID still answers
    /// `get_lid_mapping`): the most recent by `updated_at`, ties by the row's age.
    async fn get_pn_mapping(&self, phone: &str) -> Result<Option<LidPnMappingEntry>> {
        const OP: &str = "get phone mapping failed";
        let phone = phone.to_owned();
        self.read(move |conn| {
            first(
                conn,
                OP,
                &format!(
                    "SELECT {LID_COLUMNS} FROM lid_pn_mapping WHERE phone = ?1 \
                     ORDER BY updated_at DESC, rowid DESC LIMIT 1"
                ),
                params![phone],
                lid_entry,
            )
        })
        .await
    }

    /// An upsert on the LID only: the same LID with another phone number moves, a new LID for a
    /// known phone number is a second row (no unique index on the phone number).
    ///
    /// `created_at` is REPLACED with the rest of the entry, as `InMemoryBackend` replaces the whole
    /// entry and as the live cache writes it (`lid_pn.rs`: a stale source stores `created_at = 0`,
    /// and the warm-up reads it). Upstream's SQLite store (`sqlite_store.rs`, the LID upsert) keeps
    /// the FIRST `created_at` and updates the phone, the source and `updated_at` only. We follow the
    /// in-memory store; a pin bump that changes either one must be looked at.
    async fn put_lid_mapping(&self, entry: &LidPnMappingEntry) -> Result<()> {
        const OP: &str = "put lid mapping failed";
        let entry = entry.clone();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                PUT_LID,
                params![
                    entry.lid,
                    entry.phone_number,
                    entry.created_at,
                    entry.updated_at,
                    entry.learning_source
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn put_lid_mappings(&self, entries: &[LidPnMappingEntry]) -> Result<()> {
        const OP: &str = "put lid mappings failed";
        let entries = entries.to_vec();
        self.write(OP, move |tx| {
            for entry in &entries {
                exec(
                    tx,
                    OP,
                    PUT_LID,
                    params![
                        entry.lid,
                        entry.phone_number,
                        entry.created_at,
                        entry.updated_at,
                        entry.learning_source
                    ],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn get_all_lid_mappings(&self) -> Result<Vec<LidPnMappingEntry>> {
        const OP: &str = "get all lid mappings failed";
        self.read(move |conn| {
            rows(
                conn,
                OP,
                &format!("SELECT {LID_COLUMNS} FROM lid_pn_mapping ORDER BY lid"),
                [],
                lid_entry,
            )
        })
        .await
    }

    // --- Base key collision detection ---

    async fn save_base_key(&self, address: &str, message_id: &str, base_key: &[u8]) -> Result<()> {
        const OP: &str = "save base key failed";
        let (address, message_id, base_key) =
            (address.to_owned(), message_id.to_owned(), base_key.to_vec());
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO base_keys(address, message_id, base_key, created_at) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(address, message_id) DO UPDATE SET base_key = excluded.base_key, \
                 created_at = excluded.created_at",
                params![address, message_id, base_key, now_secs()],
            )?;
            Ok(())
        })
        .await
    }

    async fn has_same_base_key(
        &self,
        address: &str,
        message_id: &str,
        current_base_key: &[u8],
    ) -> Result<bool> {
        const OP: &str = "check base key failed";
        let (address, message_id, current) = (
            address.to_owned(),
            message_id.to_owned(),
            current_base_key.to_vec(),
        );
        self.read(move |conn| {
            Ok(first(
                conn,
                OP,
                "SELECT base_key FROM base_keys WHERE address = ?1 AND message_id = ?2",
                params![address, message_id],
                |row| blob(row, 0),
            )?
            .is_some_and(|stored| stored == current))
        })
        .await
    }

    async fn delete_base_key(&self, address: &str, message_id: &str) -> Result<()> {
        const OP: &str = "delete base key failed";
        let (address, message_id) = (address.to_owned(), message_id.to_owned());
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM base_keys WHERE address = ?1 AND message_id = ?2",
                params![address, message_id],
            )?;
            Ok(())
        })
        .await
    }

    /// The trait's default is `Ok(0)`, but the keepalive sweep calls this for every backend: left at
    /// the default, `base_keys` would grow for ever.
    async fn delete_expired_base_keys(&self, cutoff_timestamp: i64) -> Result<u32> {
        const OP: &str = "delete expired base keys failed";
        self.sweep(OP, move |tx| {
            let gone = exec(
                tx,
                OP,
                "DELETE FROM base_keys WHERE created_at < ?1",
                params![cutoff_timestamp],
            )?;
            Ok(u32::try_from(gone).unwrap_or(u32::MAX))
        })
        .await
    }

    // --- Device registry ---

    async fn update_device_list(&self, record: DeviceListRecord) -> Result<()> {
        const OP: &str = "update device list failed";
        let json = encode_devices(&record)?;
        let user = record.user.to_string();
        self.write(OP, move |tx| {
            exec(tx, OP, PUT_DEVICES, params![user, json])?;
            Ok(())
        })
        .await
    }

    async fn update_device_lists(&self, records: Vec<DeviceListRecord>) -> Result<()> {
        const OP: &str = "update device lists failed";
        let mut encoded = Vec::with_capacity(records.len());
        for record in &records {
            encoded.push((record.user.to_string(), encode_devices(record)?));
        }
        self.write(OP, move |tx| {
            for (user, json) in &encoded {
                exec(tx, OP, PUT_DEVICES, params![user, json])?;
            }
            Ok(())
        })
        .await
    }

    async fn get_devices(&self, user: &str) -> Result<Option<DeviceListRecord>> {
        const OP: &str = "get devices failed";
        let user = user.to_owned();
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT record FROM device_registry WHERE user = ?1",
                params![user],
                |row| blob(row, 0),
            )?
            .map(|record| decode_devices(&record))
            .transpose()
        })
        .await
    }

    async fn get_devices_batch(&self, users: &[&str]) -> Result<Vec<DeviceListRecord>> {
        const OP: &str = "get devices batch failed";
        let users: Vec<String> = users.iter().map(|user| (*user).to_owned()).collect();
        self.read(move |conn| {
            let mut found = Vec::with_capacity(users.len());
            for user in &users {
                let record = first(
                    conn,
                    OP,
                    "SELECT record FROM device_registry WHERE user = ?1",
                    params![user],
                    |row| blob(row, 0),
                )?;
                if let Some(record) = record {
                    found.push(decode_devices(&record)?);
                }
            }
            Ok(found)
        })
        .await
    }

    async fn delete_devices(&self, user: &str) -> Result<()> {
        const OP: &str = "delete devices failed";
        let user = user.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM device_registry WHERE user = ?1",
                params![user],
            )?;
            Ok(())
        })
        .await
    }

    // --- Group metadata cache ---

    async fn get_group_metadata(&self, group_jid: &str) -> Result<Option<Vec<u8>>> {
        const OP: &str = "get group metadata failed";
        let group_jid = group_jid.to_owned();
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT blob FROM group_metadata WHERE group_jid = ?1",
                params![group_jid],
                |row| blob(row, 0),
            )
        })
        .await
    }

    async fn put_group_metadata(&self, group_jid: &str, blob: &[u8]) -> Result<()> {
        const OP: &str = "put group metadata failed";
        let (group_jid, blob) = (group_jid.to_owned(), blob.to_vec());
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO group_metadata(group_jid, blob) VALUES (?1, ?2) \
                 ON CONFLICT(group_jid) DO UPDATE SET blob = excluded.blob",
                params![group_jid, blob],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_group_metadata(&self, group_jid: &str) -> Result<()> {
        const OP: &str = "delete group metadata failed";
        let group_jid = group_jid.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM group_metadata WHERE group_jid = ?1",
                params![group_jid],
            )?;
            Ok(())
        })
        .await
    }

    // --- Trusted-contact tokens ---

    async fn get_tc_token(&self, jid: &str) -> Result<Option<TcTokenEntry>> {
        const OP: &str = "get tc token failed";
        let jid = jid.to_owned();
        self.read(move |conn| {
            first(
                conn,
                OP,
                &format!("SELECT {TC_COLUMNS} FROM tc_tokens WHERE jid = ?1"),
                params![jid],
                tc_entry,
            )
        })
        .await
    }

    /// In the order asked, `None` where there is no row.
    async fn get_tc_tokens(&self, jids: &[String]) -> Result<Vec<Option<TcTokenEntry>>> {
        const OP: &str = "get tc tokens failed";
        let jids = jids.to_vec();
        self.read(move |conn| {
            let mut found = Vec::with_capacity(jids.len());
            for jid in &jids {
                found.push(first(
                    conn,
                    OP,
                    &format!("SELECT {TC_COLUMNS} FROM tc_tokens WHERE jid = ?1"),
                    params![jid],
                    tc_entry,
                )?);
            }
            Ok(found)
        })
        .await
    }

    async fn put_tc_token(&self, jid: &str, entry: &TcTokenEntry) -> Result<()> {
        const OP: &str = "put tc token failed";
        let (jid, entry) = (jid.to_owned(), entry.clone());
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO tc_tokens(jid, token, token_timestamp, sender_timestamp) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(jid) DO UPDATE SET token = excluded.token, \
                 token_timestamp = excluded.token_timestamp, \
                 sender_timestamp = excluded.sender_timestamp",
                params![
                    jid,
                    entry.token,
                    entry.token_timestamp,
                    entry.sender_timestamp
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn delete_tc_token(&self, jid: &str) -> Result<()> {
        const OP: &str = "delete tc token failed";
        let jid = jid.to_owned();
        self.write(OP, move |tx| {
            exec(tx, OP, "DELETE FROM tc_tokens WHERE jid = ?1", params![jid])?;
            Ok(())
        })
        .await
    }

    async fn get_all_tc_token_jids(&self) -> Result<Vec<String>> {
        const OP: &str = "get all tc token jids failed";
        self.read(move |conn| {
            rows(
                conn,
                OP,
                "SELECT jid FROM tc_tokens ORDER BY jid",
                [],
                |row| row.get(0),
            )
        })
        .await
    }

    /// A row goes only when BOTH its received token and its sender bucket are expired or absent:
    /// recent sender state is never dropped because the received token expired. (`IS NOT NULL` is
    /// explicit: a NULL sender time compared with the cutoff would make the whole test NULL and keep
    /// the row.)
    async fn delete_expired_tc_tokens(&self, token_cutoff: i64, sender_cutoff: i64) -> Result<u32> {
        const OP: &str = "delete expired tc tokens failed";
        self.sweep(OP, move |tx| {
            let gone = exec(
                tx,
                OP,
                "DELETE FROM tc_tokens WHERE NOT ( \
                   (length(token) > 0 AND token_timestamp >= ?1) \
                   OR (sender_timestamp IS NOT NULL AND sender_timestamp >= ?2))",
                params![token_cutoff, sender_cutoff],
            )?;
            Ok(u32::try_from(gone).unwrap_or(u32::MAX))
        })
        .await
    }

    /// Required atomic merge, ONE statement: a missing row becomes a byte-less placeholder whose token
    /// time is the sender time; an existing row keeps its token bytes and time and moves its sender
    /// time forward only (`max`).
    async fn touch_tc_token_sender_timestamp(
        &self,
        jid: &str,
        sender_timestamp: i64,
    ) -> Result<()> {
        const OP: &str = "touch tc token failed";
        let jid = jid.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                TOUCH_TC_SENDER_TIMESTAMP,
                params![jid, sender_timestamp],
            )?;
            Ok(())
        })
        .await
    }

    /// Required atomic merge, ONE statement, newer wins: the token and its time are replaced only
    /// when the stored token is a placeholder (empty) or the incoming time is at least as new; the
    /// sender time is never touched; a new row has none.
    async fn store_received_tc_token(
        &self,
        jid: &str,
        token: &[u8],
        token_timestamp: i64,
    ) -> Result<()> {
        const OP: &str = "store received tc token failed";
        let (jid, token) = (jid.to_owned(), token.to_vec());
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                STORE_RECEIVED_TC_TOKEN,
                params![jid, token, token_timestamp],
            )?;
            Ok(())
        })
        .await
    }

    // --- Sent messages (retry receipts) ---

    async fn store_sent_message(
        &self,
        chat_jid: &str,
        message_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        const OP: &str = "store sent message failed";
        let (chat_jid, message_id, payload) =
            (chat_jid.to_owned(), message_id.to_owned(), payload.to_vec());
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO sent_messages(chat_jid, message_id, payload, created_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(chat_jid, message_id) DO UPDATE SET payload = excluded.payload, \
                 created_at = excluded.created_at",
                params![chat_jid, message_id, payload, now_secs()],
            )?;
            Ok(())
        })
        .await
    }

    /// A plain read: it neither removes the row nor refreshes its age. (The trait's default is an
    /// ERROR, not a fallback.)
    async fn get_sent_message(&self, chat_jid: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        const OP: &str = "get sent message failed";
        let (chat_jid, message_id) = (chat_jid.to_owned(), message_id.to_owned());
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT payload FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2",
                params![chat_jid, message_id],
                |row| blob(row, 0),
            )
        })
        .await
    }

    /// Retrieve and delete in ONE statement (`DELETE … RETURNING`): consuming prevents a double retry.
    async fn take_sent_message(&self, chat_jid: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        const OP: &str = "take sent message failed";
        let (chat_jid, message_id) = (chat_jid.to_owned(), message_id.to_owned());
        self.write(OP, move |tx| {
            first(
                tx,
                OP,
                "DELETE FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2 RETURNING payload",
                params![chat_jid, message_id],
                |row| blob(row, 0),
            )
        })
        .await
    }

    async fn delete_expired_sent_messages(&self, cutoff_timestamp: i64) -> Result<u32> {
        const OP: &str = "delete expired sent messages failed";
        self.sweep(OP, move |tx| {
            let gone = exec(
                tx,
                OP,
                "DELETE FROM sent_messages WHERE created_at < ?1",
                params![cutoff_timestamp],
            )?;
            Ok(u32::try_from(gone).unwrap_or(u32::MAX))
        })
        .await
    }

    // --- Pending inbound buffer (the durability hook): see `pending.rs` ---

    async fn store_pending_inbound(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
        message: &[u8],
    ) -> Result<()> {
        self.pending_store(chat, sender, id, message).await
    }

    async fn get_pending_inbound(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.pending_get(chat, sender, id).await
    }

    async fn delete_pending_inbound(&self, chat: &str, sender: &str, id: &str) -> Result<()> {
        self.pending_delete(chat, sender, id).await
    }

    async fn delete_expired_pending_inbound(&self, cutoff_timestamp: i64) -> Result<u32> {
        self.pending_delete_expired(cutoff_timestamp).await
    }

    async fn read_pending_inbound_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<PendingInboundRecord>> {
        self.pending_read_bounded(key, max_bytes, lease).await
    }

    async fn read_pending_inbound_group_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<(String, PendingInboundRecord)>> {
        self.pending_read_group_bounded(key, max_bytes, lease).await
    }

    async fn replace_pending_inbound_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        replacement: PendingInboundPayload,
        max_bytes: usize,
    ) -> Result<PendingInboundMutation> {
        self.pending_replace_if(key, expected, replacement, max_bytes)
            .await
    }

    async fn delete_pending_inbound_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<PendingInboundMutation> {
        self.pending_delete_if(key, expected, max_bytes, lease)
            .await
    }

    async fn store_pending_inbound_batch(&self, rows: &[PendingInboundRow<'_>]) -> Result<()> {
        self.pending_store_batch(rows).await
    }

    async fn delete_pending_inbound_batch(&self, keys: &[PendingInboundKey<'_>]) -> Result<()> {
        self.pending_delete_batch(keys).await
    }
}
