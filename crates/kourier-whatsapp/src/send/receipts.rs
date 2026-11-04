//! The receipts we send and the durable commands that only need a confirmation (W5a, chunk 11; decision D10,
//! brief §2.6).
//!
//! THE JOB. `Command::SendReceipts` is ONE zone ([`persist`]) that writes a `receipt_jobs` row and the
//! `CommandPersisted` of the command (Signal's `commands::persist_receipts`): once the core hears the
//! confirmation the receipt is safe in `backend.db`, and the core deletes its `pending_commands` row. The
//! RUNNER ([`run`]) sends the jobs while a connection is up (the actor publishes the session of a live,
//! unpaused connection on a `watch`; a new job wakes it) and deletes a job only after every write of it
//! returned Ok, so a kill or a failed write sends that receipt again later: harmless (the peer's phone shows
//! "read" once). A job whose target has no address on the wire is dropped with a log line (it could never be
//! sent).
//!
//! WHAT GOES OUT. `Read` -> upstream's `mark_as_read` (a DM's becomes `read-self` when the phone's setting
//! says so: his own setting decides, W5 states no `AccountSettings`, D18), `Viewed` -> `mark_as_played`. A 1:1
//! chat is one call without a `sender`; in a group one call per AUTHOR, the author as `sender` in the group's
//! addressing mode; the chat is written by `Addressing::chat` (D20). Our own messages get no receipt.
//! `Delivered` is upstream's own (sent on receive, W3): confirmed with no job. Receipts are NOT held back by
//! the reach-out lock (his round-2 answer).
//!
//! THE SETTING FIRST (fix F5). Whether a 1:1 receipt says `read` or `read-self` is upstream's, by the device's
//! `read_receipts_disabled`, which upstream's own background init fetches after the login in parallel with other
//! work (default `false`). A receipt written before that answer would tell the peer "read" on a phone that turned
//! read receipts off, so before the FIRST 1:1 `read`/`played` write of a live connection the runner asks the session
//! to make the setting known ([`Session::privacy_known`], [`Privacy`]); a failure keeps the job for the next
//! drain or connection and nothing is written on an unknown setting. Groups never read the setting (a group
//! receipt is always `read`).
//!
//! WHAT STOPS THE DRAIN (fix F14). Only a CONNECTIVITY failure ([`connectivity`]: no client, not connected, not logged
//! in, a socket that failed, an IQ with no answer in time or refused with the server's own trouble, 5xx or 429): the jobs wait for the next connection or job, in order. A job
//! that fails any other way (an invalid request, an internal error, a group whose info was refused for good) could never
//! succeed, so it is dropped with a log line by kind and the jobs behind it go on: one malformed target must not stop the
//! read receipts of every chat. A group's addressing mode that cannot be read is judged the same way. Jobs older than
//! 14 days ([`JOB_KEEP_MS`], by `at`) go at a start ([`sweep`]).
//!
//! D6 IN THE BACKEND (D7): a receipt into a direct chat the phone never started writes no job (no frame);
//! the command is still confirmed, there is nothing to send for it.
//!
//! THE OTHER CONFIRMATIONS. `SyncReadMarker`: noted for the history hint (`history::reads::note_in`) and
//! confirmed in the same zone; the receipt carries the read of live messages, and reads of history rows reach
//! the phone only with W6b's `markChatAsRead` (said in the docs). `DeleteAttachment`: confirmed (WhatsApp media
//! have no ids to tell).
//!
//! LOGS: kinds and counts only, never a chat, an id or a person.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kourier_backend_api::{BackendEvent, encode_outbox_row};
use kourier_db::outbox;
use kourier_db::rusqlite::{Transaction, params};
use kourier_model::{CmdId, ConvKey, MsgTarget, ReceiptKind, TargetAuthor, UserId};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use wacore::store::error::StoreError;
use wacore_binary::Jid;

use super::address::{Addressing, Namespace, Unaddressable};
use super::hold::Recheck;
use super::{known, started};
use crate::error;
use crate::map::OwnIds;
use crate::session::{ReadKind, SendFailure, Session};
use crate::store::{KourierStore, blob, chat_state, exec, rows};

/// What [`persist`] did besides the confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Persisted {
    /// A job was written for the runner.
    Job,
    /// Nothing is due: a delivery receipt (upstream sends its own) or no target written by somebody else.
    NothingDue,
    /// A direct chat the phone never started (D7): nothing goes out.
    NotStarted,
}

/// `Command::SendReceipts`: the job (when one is due) and `CommandPersisted{cmd}` in ONE zone.
pub(crate) async fn persist(
    store: &KourierStore,
    cmd: CmdId,
    conv: ConvKey,
    targets: Vec<MsgTarget>,
    kind: ReceiptKind,
    now_ms: i64,
) -> Result<Persisted, StoreError> {
    let confirmation = confirmation(cmd)?;
    let payload = serde_json::to_vec(&targets)
        .map_err(|_| error::invalid("the targets of a receipt could not be encoded"))?;
    let due = targets
        .iter()
        .any(|target| others(&target.author).is_some());
    store
        .write("persist a receipt job", move |tx| {
            let persisted = match kind {
                ReceiptKind::Delivered => Persisted::NothingDue,
                _ if !due => Persisted::NothingDue,
                _ if !started::holds(tx, &conv)? => Persisted::NotStarted,
                ReceiptKind::Read | ReceiptKind::Viewed => {
                    exec(
                        tx,
                        "write a receipt job",
                        "INSERT INTO receipt_jobs(cmd, conv, kind, targets, at) VALUES (?1, ?2, ?3, ?4, ?5) \
                         ON CONFLICT (cmd) DO NOTHING",
                        params![cmd.0, conv.encode(), kind_name(kind), payload, now_ms],
                    )?;
                    Persisted::Job
                }
            };
            append(tx, &confirmation, now_ms)?;
            Ok(persisted)
        })
        .await
}

/// `Command::SyncReadMarker`: noted for the history hint and confirmed, in one zone.
pub(crate) async fn confirm_read_marker(
    store: &KourierStore,
    cmd: CmdId,
    conv: &ConvKey,
    now_ms: i64,
) -> Result<(), StoreError> {
    let confirmation = confirmation(cmd)?;
    let key = conv.encode();
    store
        .write("confirm a read marker", move |tx| {
            crate::history::reads::note_in(tx, &key)?;
            append(tx, &confirmation, now_ms)
        })
        .await
}

/// A durable command with nothing to carry out on WhatsApp (`DeleteAttachment`): confirmed in a zone of its own.
pub(crate) async fn confirm(
    store: &KourierStore,
    cmd: CmdId,
    now_ms: i64,
) -> Result<(), StoreError> {
    let confirmation = confirmation(cmd)?;
    store
        .write("confirm a command", move |tx| {
            append(tx, &confirmation, now_ms)
        })
        .await
}

/// `Command::ClearConversation` (W6a1, D14): the chat's upload records go (`media_uploads WHERE conv = ?`: the clear
/// removed the messages their files belong to) and the command is confirmed, in ONE zone. The phone is not told (W6b
/// writes `clearChat`/`deleteChat` with the range, behind the account's switch): WhatsApp states `delete_for_me: false`,
/// so the dialog never promised the other devices.
pub(crate) async fn confirm_clear(
    store: &KourierStore,
    cmd: CmdId,
    conv: &ConvKey,
    cut_ms: Option<i64>,
    now_ms: i64,
) -> Result<(), StoreError> {
    let confirmation = confirmation(cmd)?;
    let conv = conv.clone();
    store
        .write("confirm a clear of a chat", move |tx| {
            super::record::forget_chat(tx, &conv)?;
            // The cut of the chat (F3, INTEROP-4), as the core keeps it for its own clear: the larger of the command's two
            // times (what went), else the time of this confirmation (nothing went: the chat held nothing yet).
            chat_state::raise_cut(tx, &conv, cut_ms.unwrap_or(now_ms))?;
            append(tx, &confirmation, now_ms)
        })
        .await
}

fn confirmation(cmd: CmdId) -> Result<Vec<u8>, StoreError> {
    encode_outbox_row(&[BackendEvent::CommandPersisted { cmd }])
        .map_err(|_| error::invalid("CommandPersisted could not be encoded"))
}

fn append(tx: &Transaction<'_>, row: &[u8], now_ms: i64) -> Result<(), StoreError> {
    outbox::append(tx, row, now_ms)
        .map(drop)
        .map_err(|_| error::database("append CommandPersisted failed"))
}

fn kind_name(kind: ReceiptKind) -> &'static str {
    match kind {
        ReceiptKind::Delivered => "delivered",
        ReceiptKind::Read => "read",
        ReceiptKind::Viewed => "viewed",
    }
}

/// The author of a target when it is somebody else (our own messages get no receipt).
fn others(author: &TargetAuthor) -> Option<&UserId> {
    match author {
        TargetAuthor::User(user) => Some(user),
        TargetAuthor::Own | TargetAuthor::Unknown => None,
    }
}

/// One job as the runner reads it.
#[derive(Debug, Clone)]
pub(crate) struct Job {
    cmd: i64,
    conv: ConvKey,
    kind: ReadKind,
    targets: Vec<MsgTarget>,
}

/// One write of a job: `mark_as_read`/`mark_as_played` of `ids` in `chat`, by `sender` in a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) chat: Jid,
    pub(crate) sender: Option<Jid>,
    pub(crate) ids: Vec<String>,
}

/// The writes of `job`: one in a 1:1 chat, one per author in a group (`group` = its addressing mode, when
/// known). Our own messages and targets of another chat are left out. PURE.
pub(crate) fn calls(
    job: &Job,
    addressing: &Addressing<'_>,
    group: Option<Namespace>,
) -> Result<Vec<Call>, Unaddressable> {
    let chat = addressing.chat(&job.conv)?;
    let mut by_author: BTreeMap<String, (Option<Jid>, Vec<String>)> = BTreeMap::new();
    for target in &job.targets {
        if target
            .conv
            .as_ref()
            .is_some_and(|stated| stated != &job.conv)
        {
            continue;
        }
        let Some(author) = others(&target.author) else {
            continue;
        };
        if addressing.own.is_own(author) {
            continue;
        }
        let sender = match job.conv {
            ConvKey::Group(_) => Some(addressing.person_in(author, group)?),
            _ => None,
        };
        let key = sender.as_ref().map(ToString::to_string).unwrap_or_default();
        let entry = by_author.entry(key).or_insert_with(|| (sender, Vec::new()));
        entry.1.push(target.local.to_string());
    }
    Ok(by_author
        .into_values()
        .map(|(sender, ids)| Call {
            chat: chat.clone(),
            sender,
            ids,
        })
        .collect())
}

/// Every job, oldest command first. A row that does not decode is returned as `Err(cmd)`: it is dropped.
async fn jobs(store: &KourierStore) -> Result<Vec<Result<Job, i64>>, StoreError> {
    store
        .read(|conn| {
            rows(
                conn,
                "read the receipt jobs",
                "SELECT cmd, conv, kind, targets FROM receipt_jobs ORDER BY cmd",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        blob(r, 3)?,
                    ))
                },
            )
        })
        .await
        .map(|found| {
            found
                .into_iter()
                .map(|(cmd, conv, kind, targets)| {
                    let conv = ConvKey::parse(&conv).map_err(|_| cmd)?;
                    let kind = match kind.as_str() {
                        "read" => ReadKind::Read,
                        "viewed" => ReadKind::Played,
                        _ => return Err(cmd),
                    };
                    let targets = serde_json::from_slice(&targets).map_err(|_| cmd)?;
                    Ok(Job {
                        cmd,
                        conv,
                        kind,
                        targets,
                    })
                })
                .collect()
        })
}

async fn delete(store: &KourierStore, cmd: i64) -> Result<(), StoreError> {
    store
        .write("delete a receipt job", move |tx| {
            exec(
                tx,
                "delete a receipt job",
                "DELETE FROM receipt_jobs WHERE cmd = ?1",
                [cmd],
            )
            .map(drop)
        })
        .await
}

/// How long a receipt job may wait for a connection: 14 days, by its `at` (fix F14).
pub(crate) const JOB_KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// Forgets the jobs older than [`JOB_KEEP_MS`] before `now_ms`, in the caller's zone (the start sweep). Returns how many
/// went.
pub(crate) fn sweep(tx: &Transaction<'_>, now_ms: i64) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget old receipt jobs",
        "DELETE FROM receipt_jobs WHERE at < ?1",
        [now_ms.saturating_sub(JOB_KEEP_MS)],
    )
}

/// Whether `failure` is about the CONNECTION or the SERVER's state (a later try may succeed with the same job) and
/// not about the job (fix F14). Only these stop a drain: no client, not connected, not logged in, a socket that
/// failed, an IQ with no answer in time, and an IQ the server refused with a status that is the server's own trouble
/// (5xx, 429). A refusal of 4xx (a group the account left) would come again at every drain.
pub(crate) fn connectivity(failure: SendFailure) -> bool {
    match failure {
        SendFailure::NoClient
        | SendFailure::NotConnected
        | SendFailure::NotLoggedIn
        | SendFailure::Socket
        | SendFailure::IqTimeout => true,
        SendFailure::IqRefused { code } => code == 429 || code >= 500,
        _ => false,
    }
}

/// How a [`drain`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drained {
    /// Every job was sent (or dropped as unsendable: no address, or a failure of the job itself, fix F14).
    All,
    /// The connection or the store failed: the jobs left wait for the next connection or job.
    Stopped,
}

/// Whether the live connection's read-receipts setting is known to the client (fix F5): asked once per connection,
/// before the first 1:1 `read`/`played` write, and forgotten when the runner sees the connection gone. A drop and a
/// reconnect that the runner never sees as gone (the `watch` coalesces them) keep it: the setting upstream persisted
/// at the last fetch then stands, as it did before this fix; only a connection's FIRST receipts were unsafe.
#[derive(Debug, Default)]
pub(crate) struct Privacy(AtomicBool);

impl Privacy {
    /// The connection ended: the next one asks again.
    pub(crate) fn forget(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    /// `Ok` when the setting is known (asked now, or earlier on this connection).
    async fn ensure(&self, session: &dyn Session) -> Result<(), SendFailure> {
        if self.0.load(Ordering::SeqCst) {
            return Ok(());
        }
        session.privacy_known().await?;
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Sends every job over `session` in order and deletes each after its writes returned Ok.
pub(crate) async fn drain(
    store: &KourierStore,
    session: &dyn Session,
    own: &OwnIds,
    privacy: &Privacy,
) -> Drained {
    let found = match jobs(store).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "the receipt jobs could not be read");
            return Drained::Stopped;
        }
    };
    if found.is_empty() {
        return Drained::All;
    }
    let migrated = session.lid_migrated().await;
    'jobs: for job in found {
        let job = match job {
            Ok(job) => job,
            Err(cmd) => {
                tracing::warn!("a receipt job that could not be read was dropped");
                if delete(store, cmd).await.is_err() {
                    return Drained::Stopped;
                }
                continue;
            }
        };
        let mut users: Vec<UserId> = job
            .targets
            .iter()
            .filter_map(|target| others(&target.author).cloned())
            .collect();
        if let ConvKey::Direct(user) = &job.conv {
            users.push(user.clone());
        }
        let known = match store
            .read(move |conn| known::load(conn, users.iter()))
            .await
        {
            Ok(known) => known,
            Err(e) => {
                tracing::warn!(error = %e, "a receipt job could not read its store");
                return Drained::Stopped;
            }
        };
        let group = match &job.conv {
            ConvKey::Group(_) => match super::address::jid_of(&job.conv) {
                Ok(jid) => match session.group_mode(jid).await {
                    Ok(mode) => Some(mode),
                    // The mode decides how the author is written: not planning with a guess (fix F14).
                    Err(failure) if connectivity(failure) => {
                        tracing::info!(
                            ?failure,
                            "a group's addressing mode is not known yet; the receipts wait"
                        );
                        return Drained::Stopped;
                    }
                    Err(failure) => {
                        tracing::warn!(
                            ?failure,
                            "a receipt for a group whose mode cannot be read was dropped"
                        );
                        if delete(store, job.cmd).await.is_err() {
                            return Drained::Stopped;
                        }
                        continue 'jobs;
                    }
                },
                Err(_) => None,
            },
            _ => None,
        };
        // The addressing borrows the pairs; it lives only for the plan (no await meanwhile).
        let planned = {
            let addressing = Addressing {
                migrated,
                map: &known,
                own,
            };
            calls(&job, &addressing, group)
        };
        let planned = match planned {
            Ok(planned) => planned,
            Err(reason) => {
                tracing::warn!(?reason, "a receipt with no address on WhatsApp was dropped");
                if delete(store, job.cmd).await.is_err() {
                    return Drained::Stopped;
                }
                continue;
            }
        };
        // A 1:1 receipt is `read` or `read-self` by the setting (fix F5): known before the first one goes out.
        if !planned.is_empty()
            && matches!(job.conv, ConvKey::Direct(_))
            && let Err(failure) = privacy.ensure(session).await
        {
            tracing::info!(
                ?failure,
                "the read-receipt setting is not known yet; the receipts wait"
            );
            return Drained::Stopped;
        }
        for call in planned {
            if let Err(failure) = session
                .mark_read(call.chat, call.sender, call.ids, job.kind)
                .await
            {
                if connectivity(failure) {
                    tracing::info!(?failure, kind = ?job.kind, "a receipt was not sent; it waits");
                    return Drained::Stopped;
                }
                // A failure of the job itself would come again at every drain and hold back every later job (fix F14).
                tracing::warn!(?failure, kind = ?job.kind, "a receipt that cannot be sent was dropped");
                if delete(store, job.cmd).await.is_err() {
                    return Drained::Stopped;
                }
                continue 'jobs;
            }
        }
        if let Err(e) = delete(store, job.cmd).await {
            tracing::warn!(error = %e, "a sent receipt job could not be deleted");
            return Drained::Stopped;
        }
    }
    Drained::All
}

/// The session receipts go out over: `Some` while a connection is up and not paused (the actor keeps it).
pub(crate) type Live = watch::Receiver<Option<Arc<dyn Session>>>;

/// THE RUNNER: drains the jobs whenever a live session appears or changes and whenever a job is written
/// (`wake`), until `cancel` or the actor's end. Our own ids come from `own` at each drain.
pub(crate) async fn run<F, O>(
    store: KourierStore,
    mut live: Live,
    wake: Arc<Notify>,
    own: F,
    cancel: CancellationToken,
) where
    F: Fn() -> O + Send,
    O: std::future::Future<Output = OwnIds> + Send,
{
    let privacy = Privacy::default();
    loop {
        let current = live.borrow_and_update().clone();
        if let Some(session) = current {
            let own = own().await;
            drain(&store, session.as_ref(), &own, &privacy).await;
        } else {
            // No live connection: the next one asks for the setting again.
            privacy.forget();
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            changed = live.changed() => if changed.is_err() { return },
            () = wake.notified() => {}
        }
    }
}

/// `Command::SetTyping` over `session`, fire and forget (the actor asked the reach-out lock first: no typing
/// while it is not open, D8; `hold` asks again right before the write, fix F6). A direct chat the phone never started gets nothing (D7). A failure is logged by
/// kind only.
pub(crate) async fn typing(
    store: KourierStore,
    session: Arc<dyn Session>,
    own: OwnIds,
    conv: ConvKey,
    on: bool,
    hold: Recheck,
) {
    let users: Vec<UserId> = match &conv {
        ConvKey::Direct(user) => vec![user.clone()],
        _ => Vec::new(),
    };
    let looked = {
        let conv = conv.clone();
        store
            .read(move |conn| {
                Ok((
                    started::holds(conn, &conv)?,
                    known::load(conn, users.iter())?,
                ))
            })
            .await
    };
    let (started, known) = match looked {
        Ok(looked) => looked,
        Err(e) => {
            tracing::warn!(error = %e, "a typing state could not read its store");
            return;
        }
    };
    if !started {
        tracing::debug!("no typing state into a chat the phone has not started");
        return;
    }
    let migrated = session.lid_migrated().await;
    let chat = {
        let addressing = Addressing {
            migrated,
            map: &known,
            own: &own,
        };
        addressing.chat(&conv)
    };
    let Ok(chat) = chat else {
        tracing::debug!("no typing state into a chat with no address on WhatsApp");
        return;
    };
    // THE HOLD AGAIN (fix F6): the store and the account's mode were awaited since the actor looked at the gate.
    if !hold.open() {
        tracing::debug!("no typing state: the reach-out lock was pushed meanwhile");
        return;
    }
    if let Err(failure) = session.typing(chat, on).await {
        tracing::debug!(?failure, "a typing state was not sent");
    }
}

#[cfg(test)]
mod tests;
