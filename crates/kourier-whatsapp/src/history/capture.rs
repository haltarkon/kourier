//! The capture commit (W4 C4): one chunk into the stage, in ONE zone, before the phone is told.
//!
//! Upstream awaits the capture hook before it sends the chunk's `hist_sync` receipt, and keeps no
//! replay buffer, so `Ok` must mean "the bytes are on disk": [`Capture::commit`] returns only after the
//! stage's `COMMIT` (kill points `wa_inside_history_capture` and `wa_after_history_capture` around
//! it). It decodes nothing: upstream runs the capture before decompression and protobuf validation,
//! and a blob that does not read is found by the feeder's first pass and marked `bad`.
//!
//! IDEMPOTENT BY MESSAGE ID: upstream can call the hook again for a notification it already
//! captured (a duplicate delivery, or a receipt that failed to send). The second commit of an id
//! changes nothing, whatever state the first one has reached (`ON CONFLICT DO NOTHING`; never
//! `INSERT OR REPLACE`, which would put a delivered chunk back to `captured` and deliver it twice).
//! Two commits of one id from two tasks are one row for the same reason.
//!
//! [`CaptureHook`] is the adapter upstream calls (`HistorySyncCaptureHook`), registered ONLY in the
//! builder module (`session::build_bot`). It does three things before the commit: it refuses while the
//! login's gate refused a pairing (a refused identity's history is never kept), it names the stage's
//! owner (the account's own id, read from the stored device once), and it keeps NO reference to the
//! `Client` it is handed (never two clients on one store: a capture task still running for a client
//! that was replaced can only write a row, and a row of an id already there changes nothing). An
//! error ends nothing: upstream withholds the receipt and goes on, a reconnect buys nothing because
//! it keeps no replay buffer, and the failure is counted by kind and logged without an id.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use kourier_db::Priority;
use tokio::sync::{Notify, OnceCell};
use wacore::store::error::StoreError;
use wacore::store::traits::DeviceStore as _;
use waproto::whatsapp as wa;
use whatsapp_rust::Client;
use whatsapp_rust::types::durability_hook::HistorySyncCaptureHook;

use super::stage::LazyStage;
use crate::db::Points;
use crate::error::{self, sql};
use crate::failpoints::{AFTER_HISTORY_CAPTURE, INSIDE_HISTORY_CAPTURE};
use crate::store::KourierStore;
use crate::tap::Tap;

/// What a commit did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Captured {
    /// The chunk is new: its bytes are in the stage now.
    New,
    /// The id was captured before; nothing changed.
    Duplicate,
}

/// The capture of one account: the stage it writes to and the wake of the driver that delivers.
#[derive(Clone)]
pub(crate) struct Capture {
    stage: Arc<LazyStage>,
    wake: Arc<Notify>,
}

impl Capture {
    /// A capture into `stage`; `wake` is notified after each NEW chunk is durable.
    pub(crate) fn new(stage: Arc<LazyStage>, wake: Arc<Notify>) -> Self {
        Self { stage, wake }
    }

    /// Makes the chunk `msg_id` (the id of the notice that announced it) durable. `owner` names the
    /// account for a stage that is made now; `sync_type` is upstream's `HistorySyncType` number when it
    /// has one; `bytes` is the zlib `HistorySync` exactly as the phone uploaded it.
    ///
    /// `Ok` only after the stage committed. An error names the operation and nothing else: the
    /// caller answers upstream with `Err`, so no receipt goes and nothing else happens.
    pub(crate) async fn commit(
        &self,
        owner: &str,
        msg_id: &str,
        sync_type: Option<i32>,
        bytes: Vec<u8>,
    ) -> Result<Captured, StoreError> {
        // Opening a stage that is not there yet makes a directory and a file and migrates it: not on
        // a worker of the client's runtime.
        let stage = Arc::clone(&self.stage);
        let owner = owner.to_owned();
        let handle = tokio::task::spawn_blocking(move || stage.handle(&owner))
            .await
            .map_err(|_| error::job_failed())?
            .map_err(|_| error::database("the history stage could not be opened"))?;

        let id = msg_id.to_owned();
        let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let points = Points {
            inside: INSIDE_HISTORY_CAPTURE,
            after: AFTER_HISTORY_CAPTURE,
        };
        let changed = handle
            .write_at(
                Priority::High,
                "capture a history chunk",
                Some(points),
                move |tx| {
                    // The row and its bytes in the ONE zone (MEDIUM-1: the blob is not in the row that
                    // every look at a chunk's state reads and every state change rewrites).
                    let changed = tx
                        .prepare_cached(
                            "INSERT INTO chunks (msg_id, sync_type, captured_at, size, state) \
                             VALUES (?1, ?2, ?3, ?4, 'captured') ON CONFLICT (msg_id) DO NOTHING",
                        )
                        .and_then(|mut s| s.execute((id, sync_type, kourier_db::now_ms(), size)))
                        .map_err(sql("capture a history chunk"))?;
                    if changed > 0 {
                        tx.prepare_cached("INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, ?2)")
                            .and_then(|mut s| s.execute((tx.last_insert_rowid(), bytes)))
                            .map_err(sql("capture the bytes of a history chunk"))?;
                    }
                    Ok(changed)
                },
            )
            .await?;
        if changed == 0 {
            return Ok(Captured::Duplicate);
        }
        self.wake.notify_one();
        Ok(Captured::New)
    }

    /// Finishes the queued stage jobs and stops the stage's thread (the actor's end): BLOCKS, so a task calls it
    /// from a blocking context. Later commits fail.
    pub(crate) fn close(&self) {
        self.stage.close();
    }
}

/// Why a chunk was not kept. The word is what the log line says; never an id or a byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failed {
    /// The login's gate refused a pairing of this client: its history is never kept.
    Refused,
    /// The account's own id could not be read from the stored device.
    Owner,
    /// The stage could not be opened or written.
    Stage,
}

impl Failed {
    #[cfg(test)]
    const ALL: [Self; 3] = [Self::Refused, Self::Owner, Self::Stage];

    fn word(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Owner => "owner",
            Self::Stage => "stage",
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::Refused => 0,
            Self::Owner => 1,
            Self::Stage => 2,
        }
    }
}

/// How many chunks were not kept in this run, by kind.
#[derive(Debug, Default)]
pub(crate) struct Failures([AtomicU64; 3]);

impl Failures {
    fn note(&self, kind: Failed) -> u64 {
        self.0[kind.slot()].fetch_add(1, Ordering::Relaxed) + 1
    }

    /// How many chunks failed for `kind`.
    #[cfg(test)]
    pub(crate) fn count(&self, kind: Failed) -> u64 {
        self.0[kind.slot()].load(Ordering::Relaxed)
    }

    /// All kinds together.
    #[cfg(test)]
    pub(crate) fn total(&self) -> u64 {
        Failed::ALL.iter().map(|kind| self.count(*kind)).sum()
    }
}

/// The account's own id, the stage's owner: the LID user of the stored device, else the number's. The capture
/// and the driver both name the stage by it, so a stage made by one is found by the other.
///
/// Each candidate is filtered for an empty user BEFORE the fallback (LOW-2): a pair-success without a `lid`
/// attribute stores a LID with no user, and that must fall through to the number, not end the owner.
pub(crate) fn owner_of(device: &wacore::store::device::Device) -> Option<String> {
    let user =
        |jid: &wacore_binary::Jid| Some(jid.user.to_string()).filter(|user| !user.is_empty());
    device
        .lid
        .as_ref()
        .and_then(user)
        .or_else(|| device.pn.as_ref().and_then(user))
}

/// The stage's owner, decided ONCE and kept in `backend.db` (LOW-2): read back when it was decided, else taken
/// from the stored device ([`owner_of`]) and written. Never recomputed at an open: learning the LID later must not
/// turn the stage of the number into a foreign one. `None` before the account has a device.
pub(crate) async fn decided_owner(store: &KourierStore) -> Result<Option<String>, StoreError> {
    let kept = store
        .read(|conn| {
            conn.query_row("SELECT owner FROM history_cursor WHERE id = 1", [], |r| {
                r.get::<_, Option<String>>(0)
            })
            .map_err(sql("read the history stage's owner"))
        })
        .await?;
    if kept.is_some() {
        return Ok(kept);
    }
    let Some(owner) = store.load().await?.as_ref().and_then(owner_of) else {
        return Ok(None);
    };
    store
        .write("decide the history stage's owner", move |tx| {
            tx.execute(
                "UPDATE history_cursor SET owner = ?1 WHERE id = 1 AND owner IS NULL",
                [&owner],
            )
            .map_err(sql("decide the history stage's owner"))?;
            tx.query_row("SELECT owner FROM history_cursor WHERE id = 1", [], |r| {
                r.get::<_, Option<String>>(0)
            })
            .map_err(sql("read the history stage's owner"))
        })
        .await
}

/// The capture hook of one client.
pub(crate) struct CaptureHook {
    capture: Capture,
    /// Where the account's own id is read from (once).
    store: KourierStore,
    /// The login's gate, through the tap.
    tap: Tap,
    owner: OnceCell<String>,
    failures: Arc<Failures>,
}

impl CaptureHook {
    pub(crate) fn new(capture: Capture, store: KourierStore, tap: Tap) -> Self {
        Self {
            capture,
            store,
            tap,
            owner: OnceCell::new(),
            failures: Arc::new(Failures::default()),
        }
    }

    /// The failure counts, shared with the hook (a test reads them after the hook is gone).
    #[cfg(test)]
    pub(crate) fn failures(&self) -> Arc<Failures> {
        Arc::clone(&self.failures)
    }

    /// The account's own id, the stage's owner: the LID user of the stored device, else the number's.
    async fn owner(&self) -> Result<&str, Failed> {
        self.owner
            .get_or_try_init(|| async {
                decided_owner(&self.store)
                    .await
                    .ok()
                    .flatten()
                    .ok_or(Failed::Owner)
            })
            .await
            .map(String::as_str)
    }

    /// Everything the hook does except answering upstream: keeps the chunk, or says why not. The
    /// failure is counted and logged here, by kind.
    pub(crate) async fn keep(
        &self,
        msg_id: &str,
        sync_type: Option<i32>,
        bytes: &[u8],
    ) -> Result<Captured, Failed> {
        let outcome = self.try_keep(msg_id, sync_type, bytes).await;
        if let Err(kind) = outcome {
            let total = self.failures.note(kind);
            tracing::warn!(
                kind = kind.word(),
                failed_so_far = total,
                "a history chunk was not kept; its receipt is withheld"
            );
        }
        outcome
    }

    async fn try_keep(
        &self,
        msg_id: &str,
        sync_type: Option<i32>,
        bytes: &[u8],
    ) -> Result<Captured, Failed> {
        if self.tap.is_refused() {
            return Err(Failed::Refused);
        }
        let owner = self.owner().await?;
        let announced_earlier = self.announced_before(msg_id).await;
        let captured = self
            .capture
            .commit(owner, msg_id, sync_type, bytes.to_vec())
            .await
            .map_err(|_| Failed::Stage)?;
        if captured == Captured::New {
            // The one fixed line a run tool reads (T1): whether the notice of this chunk was recorded
            // before the chunk arrived, which is how a re-upload after a hard stop shows. No id, no size.
            tracing::info!(announced_earlier, "history chunk captured");
        }
        Ok(captured)
    }

    /// Whether stage 2 recorded the notice of `msg_id` already (`history_announced`, `lost` or not). A
    /// store that cannot be read says no: this is a line in a log, never a reason to keep nothing.
    async fn announced_before(&self, msg_id: &str) -> bool {
        let id = msg_id.to_owned();
        self.store
            .read(move |conn| {
                conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM history_announced WHERE msg_id = ?1)",
                    [&id],
                    |r| r.get::<_, bool>(0),
                )
                .map_err(sql("look for an announced history chunk"))
            })
            .await
            .unwrap_or(false)
    }
}

#[async_trait]
impl HistorySyncCaptureHook for CaptureHook {
    async fn on_history_sync(
        &self,
        _client: Arc<Client>,
        message_id: &str,
        sync_type: Option<wa::message::HistorySyncType>,
        compressed: &[u8],
    ) -> anyhow::Result<()> {
        // The client is not kept (module docs); the error is static (upstream logs it with `{:?}`).
        self.keep(message_id, sync_type.map(|t| t as i32), compressed)
            .await
            .map(drop)
            .map_err(|_| anyhow::anyhow!("the history chunk was not kept"))
    }
}

#[cfg(test)]
mod tests;
