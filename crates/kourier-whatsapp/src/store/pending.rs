//! The pending-inbound buffer of `ProtocolStore`: a decrypted message waits here, keyed by
//! `(chat, sender, id)`, until the inbound durability hook has committed it. The trait's defaults for
//! this family FAIL CLOSED (an unsupported backend never degrades to at-most-once), and the client
//! builder refuses a store without it when a hook is set, so for Kourier these methods are required.
//!
//! The rules, from upstream's trait and from `InMemoryBackend`, which is the reference:
//!
//! - A bounded read looks at `typeof` and `length` before it takes the bytes: a row that is no BLOB or
//!   is longer than `max_bytes` is an ERROR, never absence; empty bytes are a present row. The check
//!   and the read are one statement, so they see one row.
//! - `replace_pending_inbound_if` validates the REPLACEMENT's length first; a row already equal to the
//!   replacement is an idempotent `Applied` even when `expected` is stale (a retry after a dropped
//!   caller finds its own payload installed); only then is `expected` compared (`Conflict`), and a
//!   real replacement refreshes `inserted_at`.
//! - `delete_pending_inbound_if` has NO such shortcut: the stored revision must equal `expected`.
//!   Missing expected against an absent row is a no-op `Applied`; a present expected against
//!   absence is a `Conflict`.
//! - The group read discovers at most 401 same-chat/id sender spellings of at most 128 bytes
//!   (`pending_group_sender` picks the unique physical row, and refuses ambiguity and every limit)
//!   and reads that row, in one read transaction.
//! - Every job owns its lease until it ends, so a caller that drops its future leaves the row's
//!   owner protected for as long as the job lives.
//!
//! Each operation is ONE job, and each conditional one reads and writes in the same transaction.

use std::time::{SystemTime, UNIX_EPOCH};

use kourier_db::rusqlite::{Connection, params};
use wacore::store::error::Result;
use wacore::store::traits::{
    PendingInboundKey, PendingInboundLease, PendingInboundMutation, PendingInboundPayload,
    PendingInboundRecord, PendingInboundRevision, PendingInboundRow, pending_group_sender,
};

use super::{KourierStore, exec, first, rows};
use crate::db::Points;
use crate::error::invalid;
use crate::failpoints;

const CAS: Points = Points {
    inside: failpoints::INSIDE_PENDING_CAS,
    after: failpoints::AFTER_PENDING_CAS,
};

/// The first write of a stanza, before the hook sees it (`pending_store`, `pending_store_batch`):
/// upstream's step 1 of every commit. A kill here leaves the stanza unacked and the server sends it
/// again; the pair lets the receive loops (`receive_kill`) cut the pipeline at that write too.
const STORE: Points = Points {
    inside: failpoints::INSIDE_PENDING_STORE,
    after: failpoints::AFTER_PENDING_STORE,
};

/// The bounded read: the type and the length first; the bytes only for a blob that fits (`?4`).
pub(crate) const BOUNDED_READ: &str = "SELECT typeof(message), length(message), \
     CASE WHEN typeof(message) = 'blob' AND length(message) <= ?4 THEN message END \
     FROM pending_inbound_messages WHERE chat = ?1 AND sender = ?2 AND id = ?3";

/// The sender spellings of one `(chat, id)`: those longer than 128 bytes come back as NULL, which
/// `pending_group_sender` refuses. At most 401, one more than the limit.
pub(crate) const GROUP_SENDERS: &str = "SELECT CASE WHEN length(CAST(sender AS BLOB)) <= 128 \
     THEN sender END FROM pending_inbound_messages WHERE chat = ?1 AND id = ?2 LIMIT 401";

const PUT: &str = "INSERT INTO pending_inbound_messages(chat, sender, id, message, inserted_at) \
     VALUES (?1, ?2, ?3, ?4, ?5) \
     ON CONFLICT(chat, sender, id) DO UPDATE SET message = excluded.message, \
     inserted_at = excluded.inserted_at";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// A key as owned strings, to move into a job.
fn owned(key: PendingInboundKey<'_>) -> (String, String, String) {
    (
        key.chat.to_owned(),
        key.sender.to_owned(),
        key.id.to_owned(),
    )
}

/// The bytes of a row, or `None` when there is none. A row that is no BLOB, or is longer than
/// `max_bytes`, is an error (the bytes are not taken from the database in that case).
fn bounded(
    conn: &Connection,
    op: &'static str,
    chat: &str,
    sender: &str,
    id: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let max = i64::try_from(max_bytes).unwrap_or(i64::MAX);
    let row = first(
        conn,
        op,
        BOUNDED_READ,
        params![chat, sender, id, max],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        },
    )?;
    match row {
        None => Ok(None),
        Some((kind, ..)) if kind != "blob" => Err(invalid("a pending row is not a blob")),
        Some((_, length, _)) if length > max => Err(invalid("a pending row exceeds the bound")),
        Some((.., Some(bytes))) => Ok(Some(bytes)),
        Some((.., None)) => Err(invalid("a pending row could not be read")),
    }
}

/// The revision of what is stored under a key: of its bytes, or "missing".
fn revision(
    conn: &Connection,
    op: &'static str,
    (chat, sender, id): &(String, String, String),
    max_bytes: usize,
) -> Result<PendingInboundRevision> {
    Ok(match bounded(conn, op, chat, sender, id, max_bytes)? {
        Some(bytes) => PendingInboundRevision::for_bytes(&bytes),
        None => PendingInboundRevision::missing(),
    })
}

impl KourierStore {
    pub(super) async fn pending_store(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
        message: &[u8],
    ) -> Result<()> {
        const OP: &str = "store pending inbound failed";
        let (chat, sender, id, message) = (
            chat.to_owned(),
            sender.to_owned(),
            id.to_owned(),
            message.to_vec(),
        );
        self.write_at(OP, STORE, move |tx| {
            exec(tx, OP, PUT, params![chat, sender, id, message, now_secs()])?;
            Ok(())
        })
        .await
    }

    /// One transaction for the whole commit-batch of an offline drain.
    pub(super) async fn pending_store_batch(&self, batch: &[PendingInboundRow<'_>]) -> Result<()> {
        const OP: &str = "store pending inbound batch failed";
        let batch: Vec<(String, String, String, Vec<u8>)> = batch
            .iter()
            .map(|row| {
                (
                    row.chat.to_owned(),
                    row.sender.to_owned(),
                    row.id.to_owned(),
                    row.message.to_vec(),
                )
            })
            .collect();
        self.write_at(OP, STORE, move |tx| {
            let now = now_secs();
            for (chat, sender, id, message) in &batch {
                exec(tx, OP, PUT, params![chat, sender, id, message, now])?;
            }
            Ok(())
        })
        .await
    }

    pub(super) async fn pending_get(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
    ) -> Result<Option<Vec<u8>>> {
        const OP: &str = "get pending inbound failed";
        let (chat, sender, id) = (chat.to_owned(), sender.to_owned(), id.to_owned());
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT message FROM pending_inbound_messages WHERE chat = ?1 AND sender = ?2 AND id = ?3",
                params![chat, sender, id],
                |row| row.get::<_, Vec<u8>>(0),
            )
        })
        .await
    }

    pub(super) async fn pending_delete(&self, chat: &str, sender: &str, id: &str) -> Result<()> {
        const OP: &str = "delete pending inbound failed";
        let (chat, sender, id) = (chat.to_owned(), sender.to_owned(), id.to_owned());
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM pending_inbound_messages WHERE chat = ?1 AND sender = ?2 AND id = ?3",
                params![chat, sender, id],
            )?;
            Ok(())
        })
        .await
    }

    pub(super) async fn pending_delete_batch(&self, keys: &[PendingInboundKey<'_>]) -> Result<()> {
        const OP: &str = "delete pending inbound batch failed";
        let keys: Vec<_> = keys.iter().map(|key| owned(*key)).collect();
        self.write(OP, move |tx| {
            for (chat, sender, id) in &keys {
                exec(
                    tx,
                    OP,
                    "DELETE FROM pending_inbound_messages WHERE chat = ?1 AND sender = ?2 AND id = ?3",
                    params![chat, sender, id],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Older than the cutoff (`inserted_at < cutoff`), at low priority. The count is returned; the
    /// trait's default is a benign `Ok(0)` that would let the buffer grow for ever.
    pub(super) async fn pending_delete_expired(&self, cutoff_timestamp: i64) -> Result<u32> {
        const OP: &str = "delete expired pending inbound failed";
        self.sweep(OP, move |tx| {
            let gone = exec(
                tx,
                OP,
                "DELETE FROM pending_inbound_messages WHERE inserted_at < ?1",
                params![cutoff_timestamp],
            )?;
            Ok(u32::try_from(gone).unwrap_or(u32::MAX))
        })
        .await
    }

    pub(super) async fn pending_read_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<PendingInboundRecord>> {
        const OP: &str = "read pending inbound failed";
        let (chat, sender, id) = owned(key);
        self.read(move |conn| {
            Ok(bounded(conn, OP, &chat, &sender, &id, max_bytes)?
                .map(|bytes| PendingInboundRecord::new(bytes, lease)))
        })
        .await
    }

    pub(super) async fn pending_read_group_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<(String, PendingInboundRecord)>> {
        const OP: &str = "read pending inbound group failed";
        let (chat, sender, id) = owned(key);
        self.read(move |conn| {
            // One snapshot for the discovery and the payload.
            let tx = conn
                .unchecked_transaction()
                .map_err(crate::error::sql(OP))?;
            let spellings: Vec<Option<String>> =
                rows(&tx, OP, GROUP_SENDERS, params![chat, id], |row| row.get(0))?;
            let mut senders = spellings.iter().map(|spelling| spelling.as_deref());
            let Some(physical) = pending_group_sender(&mut senders, &sender)? else {
                return Ok(None);
            };
            let bytes = bounded(&tx, OP, &chat, &physical, &id, max_bytes)?
                .ok_or_else(|| invalid("a pending group row disappeared under its owner"))?;
            Ok(Some((physical, PendingInboundRecord::new(bytes, lease))))
        })
        .await
    }

    pub(super) async fn pending_replace_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        replacement: PendingInboundPayload,
        max_bytes: usize,
    ) -> Result<PendingInboundMutation> {
        const OP: &str = "replace pending inbound failed";
        // The replacement's own length first, before anything is read.
        if replacement.bytes().len() > max_bytes {
            return Err(invalid("a pending replacement exceeds the bound"));
        }
        let key = owned(key);
        self.write_at(OP, CAS, move |tx| {
            let stored = revision(tx, OP, &key, max_bytes)?;
            // An already installed identical replacement is an idempotent success, even when the
            // caller's `expected` is stale: its retry finds its own payload.
            if stored == replacement.revision() {
                return Ok(PendingInboundMutation::Applied);
            }
            if stored != expected {
                return Ok(PendingInboundMutation::Conflict);
            }
            let (chat, sender, id) = &key;
            exec(
                tx,
                OP,
                PUT,
                params![chat, sender, id, replacement.bytes(), now_secs()],
            )?;
            Ok(PendingInboundMutation::Applied)
        })
        .await
    }

    pub(super) async fn pending_delete_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<PendingInboundMutation> {
        const OP: &str = "delete pending inbound failed";
        let key = owned(key);
        self.write_at(OP, CAS, move |tx| {
            // The job owns the lease until it ends.
            let _lease = lease;
            // No idempotent shortcut: the stored revision must be the one the owner observed.
            if revision(tx, OP, &key, max_bytes)? != expected {
                return Ok(PendingInboundMutation::Conflict);
            }
            let (chat, sender, id) = &key;
            exec(
                tx,
                OP,
                "DELETE FROM pending_inbound_messages WHERE chat = ?1 AND sender = ?2 AND id = ?3",
                params![chat, sender, id],
            )?;
            Ok(PendingInboundMutation::Applied)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use futures::FutureExt;
    use kourier_db::{DbKey, Priority};
    use wacore::store::traits::ProtocolStore;

    use super::*;
    use crate::error::sql;
    use crate::store::StoreOwner;

    fn owner() -> (tempfile::TempDir, StoreOwner) {
        let dir = tempfile::tempdir().expect("temp dir");
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &DbKey::from([3; 32]),
            "wa-test",
        )
        .expect("open");
        (dir, owner)
    }

    /// The bytes of a row are taken only inside the `CASE` that has looked at the type and the length.
    /// A read that loaded the payload first and measured it after would return the same errors, so
    /// the shape of the statement is what is held.
    #[test]
    fn the_bounded_statements_look_at_the_type_and_the_length_before_they_take_bytes() {
        assert!(BOUNDED_READ.contains(
            "CASE WHEN typeof(message) = 'blob' AND length(message) <= ?4 THEN message END"
        ));
        assert_eq!(
            BOUNDED_READ.matches("message END").count(),
            1,
            "the payload is selected once, inside the CASE"
        );
        assert!(
            GROUP_SENDERS.contains("CASE WHEN length(CAST(sender AS BLOB)) <= 128 THEN sender END")
        );
        assert!(GROUP_SENDERS.ends_with("LIMIT 401"));
    }

    /// The group read looks at one `(chat, id)` through its index.
    #[tokio::test]
    async fn the_group_discovery_uses_the_index_of_chat_and_id() {
        let (_dir, owner) = owner();
        let plan = owner
            .store()
            .read(|conn| {
                let mut stmt = conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {GROUP_SENDERS}"))
                    .map_err(sql("plan"))?;
                stmt.query_map(["c", "i"], |row| row.get::<_, String>(3))
                    .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
                    .map_err(sql("plan"))
            })
            .await
            .expect("plan")
            .join(" / ");
        assert!(plan.contains("SEARCH"), "{plan}");
        assert!(plan.contains("pending_inbound_by_chat_id"), "{plan}");
        assert!(!plan.contains("SCAN"), "{plan}");
        owner.close();
    }

    /// A job owns the lease until it ends: a caller that drops its future leaves the lease
    /// where the job holds it, as upstream asks ("detached readers retain the supplied lease").
    #[tokio::test]
    async fn a_job_holds_its_lease_until_it_ends_though_the_caller_dropped_its_future() {
        let (_dir, owner) = owner();
        let store = owner.store();
        let handle = store.inner.handle.clone();
        // The writer is busy; the read queues behind it.
        let (release, held) = mpsc::channel::<()>();
        let (started_tx, started) = mpsc::channel::<()>();
        handle
            .submit(Priority::High, 0, move |_| {
                started_tx.send(()).expect("started");
                held.recv().expect("released");
            })
            .expect("queue the blocker");
        started.recv().expect("the blocker runs");
        let marker = Arc::new(());
        let key = PendingInboundKey {
            chat: "c",
            sender: "s",
            id: "i",
        };
        let future =
            store.read_pending_inbound_bounded(key, 16, PendingInboundLease::new(marker.clone()));
        // Polled once: the job is queued. Then the caller gives up.
        assert!(future.now_or_never().is_none(), "the writer is busy");
        assert_eq!(
            Arc::strong_count(&marker),
            2,
            "the queued job owns the lease"
        );
        release.send(()).expect("release");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Arc::strong_count(&marker) != 1 {
            assert!(Instant::now() < deadline, "the job never ended");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        owner.close();
    }

    /// The compare-and-swap is ONE job in the writer's queue. The writer is parked, the CAS is queued,
    /// a job that rewrites the row is queued BEHIND it, the writer is released. Run whole, the CAS
    /// finds the revision it expected (`Applied`) and the rewrite, which comes after, is what the row
    /// holds at the end. A CAS written as a read job followed by a write job would read, let the
    /// rewrite in, and then write over it (or delete it): the row would hold the CAS's own payload,
    /// or nothing. (Kill points sit around the COMMIT, so the kill loops cannot see this.)
    #[tokio::test]
    async fn a_compare_and_swap_runs_whole_before_a_job_queued_behind_it() {
        use std::future::Future;
        use std::pin::Pin;

        use bytes::Bytes;

        type Cas<'a> = Pin<Box<dyn Future<Output = Result<PendingInboundMutation>> + Send + 'a>>;
        const KEY: PendingInboundKey<'static> = PendingInboundKey {
            chat: "c",
            sender: "s",
            id: "i",
        };
        for delete in [false, true] {
            let (_dir, owner) = owner();
            let store = owner.store();
            let handle = store.inner.handle.clone();
            store
                .store_pending_inbound("c", "s", "i", b"first")
                .await
                .expect("plant the row");
            let expected = store
                .read_pending_inbound_bounded(KEY, 16, PendingInboundLease::new(Arc::new(())))
                .await
                .expect("read")
                .expect("present")
                .revision();
            // Park the writer.
            let (release, held) = mpsc::channel::<()>();
            let (started_tx, started) = mpsc::channel::<()>();
            handle
                .submit(Priority::High, 0, move |_| {
                    started_tx.send(()).expect("started");
                    held.recv().expect("released");
                })
                .expect("queue the blocker");
            started.recv().expect("the blocker runs");
            // Queue the CAS: one poll, and it is in the writer's queue.
            let mut cas: Cas<'_> = if delete {
                Box::pin(store.delete_pending_inbound_if(
                    KEY,
                    expected,
                    16,
                    PendingInboundLease::new(Arc::new(())),
                ))
            } else {
                let payload = PendingInboundPayload::new(
                    Bytes::from_static(b"mine"),
                    PendingInboundLease::new(Arc::new(())),
                );
                Box::pin(store.replace_pending_inbound_if(KEY, expected, payload, 16))
            };
            assert!(futures::poll!(&mut cas).is_pending(), "the writer is busy");
            // Queue the rewrite behind it.
            let rewrite = handle
                .submit(Priority::High, 0, |conn| {
                    conn.execute(PUT, params!["c", "s", "i", b"theirs".as_slice(), 1])
                        .map(|_| ())
                })
                .expect("queue the rewrite");
            release.send(()).expect("release");
            let what = if delete { "delete" } else { "replace" };
            assert_eq!(
                cas.await.expect(what),
                PendingInboundMutation::Applied,
                "{what}: the revision it expected was there when it ran"
            );
            rewrite.await.expect("answered").expect("rewritten");
            assert_eq!(
                store
                    .get_pending_inbound("c", "s", "i")
                    .await
                    .expect("read back"),
                Some(b"theirs".to_vec()),
                "{what}: the rewrite queued behind the CAS is the last word"
            );
            owner.close();
        }
    }
}
