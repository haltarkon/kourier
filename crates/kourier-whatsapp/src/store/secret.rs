//! `MsgSecretStore`: the 32-byte secret of each message, keyed by chat, sender and message id.

use async_trait::async_trait;
use kourier_db::rusqlite::{Transaction, params};
use wacore::store::error::Result;
use wacore::store::traits::{MsgSecretEntry, MsgSecretStore, StoredMessageSecret};

use super::{KourierStore, blob, exec, first};
use crate::db::Points;
use crate::failpoints;

const SECRETS: Points = Points {
    inside: failpoints::INSIDE_MSG_SECRETS,
    after: failpoints::AFTER_MSG_SECRETS,
};

/// The upsert: the INCOMING secret replaces (both of upstream's stores take it), the deadline and
/// the parent time merge by the rules the trait exports. The rules are written in SQL so one
/// statement is the whole read-modify-write: `0` ("never") wins for the deadline, otherwise the
/// later; the later parent time wins, so a `0` ("unknown") never clobbers a known one.
const PUT: &str = "INSERT INTO msg_secrets(chat, sender, msg_id, secret, expires_at, message_ts) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
     ON CONFLICT(chat, sender, msg_id) DO UPDATE SET \
       secret = excluded.secret, \
       expires_at = CASE WHEN expires_at = 0 OR excluded.expires_at = 0 THEN 0 \
                         ELSE max(expires_at, excluded.expires_at) END, \
       message_ts = max(message_ts, excluded.message_ts)";

/// The upsert of every entry, inside the caller's transaction: the ONE statement `put_msg_secrets` and the
/// history's delivery zone (W4, which writes a row's secrets in the zone that appends the row) both use.
pub(crate) fn put_all(tx: &Transaction<'_>, entries: &[MsgSecretEntry]) -> Result<usize> {
    const OP: &str = "put message secrets failed";
    for entry in entries {
        exec(
            tx,
            OP,
            PUT,
            params![
                &*entry.chat,
                &*entry.sender,
                &*entry.msg_id,
                entry.secret.as_slice(),
                entry.expires_at,
                entry.message_ts
            ],
        )?;
    }
    Ok(entries.len())
}

#[async_trait]
impl MsgSecretStore for KourierStore {
    async fn put_msg_secrets(&self, entries: Vec<MsgSecretEntry>) -> Result<usize> {
        const OP: &str = "put message secrets failed";
        self.write_at(OP, SECRETS, move |tx| put_all(tx, &entries))
            .await
    }

    async fn get_stored_msg_secret(
        &self,
        chat: &str,
        sender: &str,
        msg_id: &str,
    ) -> Result<Option<StoredMessageSecret>> {
        const OP: &str = "get message secret failed";
        let (chat, sender, msg_id) = (chat.to_owned(), sender.to_owned(), msg_id.to_owned());
        self.read(move |conn| {
            let row = first(
                conn,
                OP,
                "SELECT secret, message_ts FROM msg_secrets \
                 WHERE chat = ?1 AND sender = ?2 AND msg_id = ?3",
                params![chat, sender, msg_id],
                |row| Ok((blob(row, 0)?, row.get::<_, i64>(1)?)),
            )?;
            // Malformed bytes (a length other than 32) are `StoreError::InvalidMessageSecret`,
            // naming only the length found.
            row.map(|(secret, message_ts)| {
                StoredMessageSecret::from_stored_bytes(&secret, message_ts)
            })
            .transpose()
        })
        .await
    }

    async fn delete_expired_msg_secrets(&self, cutoff_timestamp: i64) -> Result<u32> {
        const OP: &str = "delete expired message secrets failed";
        self.sweep(OP, move |tx| {
            // `expires_at = 0` is "never": the partial index only holds the rows that can expire.
            let gone = exec(
                tx,
                OP,
                "DELETE FROM msg_secrets WHERE expires_at > 0 AND expires_at <= ?1",
                params![cutoff_timestamp],
            )?;
            Ok(u32::try_from(gone).unwrap_or(u32::MAX))
        })
        .await
    }
}
