//! The outbox in `core.db` (§4.8): enqueue, attempts, outcomes, user retry and cancel, drafts.
//!
//! Every function here is one `core.db` transaction on the writer thread; the per-account
//! [`Dispatcher`](crate::dispatch::Dispatcher) decides when they run. An outbox row holds the
//! [`OutgoingRequest`] with the network id allocated once at enqueue (`local`, `sent_at`); every
//! attempt, retry and restart reuses it, so receivers dedup by id. Per-recipient outcomes go to
//! `send_state` in the same transaction as the row's state change, and a resend only ever
//! targets recipients that are not `Sent` there.
//!
//! Row states: `Queued` (waiting; `next_attempt_at` is the backoff), `Transmitting` (one
//! `Command::Transmit` in flight; back to `Queued` at the next start, or after a backoff when its
//! outcome could not be recorded), `Failed` (terminal until the user retries; kept so "Retry"
//! has the request, and never blocks the queue) and `Staged` (a row of a large send of files
//! whose messages are still being written, [`enqueue_files`]: invisible to the dispatcher
//! until the whole send is stored, and dropped at the next start after a kill). A message
//! that reached some recipients while
//! others failed for good keeps a `Failed` row with `only` = those members. A row leaves the
//! outbox when every recipient has it, when the user cancels it, or when an own copy from another
//! device arrives (echo dedup in ingest). A row can go, or be moved by a merge of two
//! conversations, while its transmit is in flight: row ids are never reused, the answer is
//! recorded where the row is then, and without the row only the per-recipient outcome of a send
//! is kept for its message.
//!
//! Edits, deletes and reactions are rows of their target message too (`kind` = `Edit`, `Delete`,
//! `React`; [`crate::modify`]): in the same per-conversation FIFO, with their own network id,
//! budget and backoff, but no `send_state` (a partial attempt narrows their own `only`); refused
//! for good, they are rolled back and dropped instead of kept as `Failed`. Once a message is a
//! tombstone (deleted here, on another device or by its author), its rows other than deletes
//! are dropped instead of sent or kept: at once, or with the answer of a transmit in flight.
//!
//! Nothing about a message is told to people who never got it: while the own send of a message
//! is `Failed` and no recipient has it, its queued edits and reactions are held (they are not
//! the head of their conversation and stay local); "Retry" of the message sends it first and
//! then them, in order. A `Failed` row says "nobody has it" for sure only when the network
//! refused the message; one that gave up when its budget ran out (retryable) ended on attempts
//! without a definite answer, so a delete for everyone of such a message is sent and an edit
//! of it is a revision ([`crate::actions::sending_of`]).
//!
//! The numbers of the outbox are a network's ([`OutboxPolicy`], `NetworkDescriptor::outbox`,
//! carried as [`NetRules::outbox`]): the retry budget, the longest back-off and how long a
//! stored challenge can still be answered. Signal's are Signal Desktop's (one day, 15 minutes,
//! one day: [`BUDGET`], [`MAX_BACKOFF`], the default policy), and no server asks for them: the
//! receive windows of the other Signal clients are built on them (an edit is taken up to 48 h
//! after its original and may be sent for 24 h, a delete up to a day after its send window,
//! `SD/ts/util/canDeleteForEveryone.preload.ts` `MESSAGE_SEND_GRACE_PERIOD`), so a change that
//! left later would be dropped by its receivers, and a message that went out days after the
//! user saw it fail would surprise them. The curve of the back-off (0, 190 ms, ×1.9) is the
//! core's for every network; only its cap is the network's ([`backoff_capped`]). Every site
//! reads the policy of the account's network, none the constants; a network that has not said
//! otherwise has Signal's. Nothing is refused by the budget: the row stays
//! for "Retry", which starts a new budget with the same id. The waits for the user count
//! against it, the one for a challenge answer and the one for a safety number to be
//! confirmed ([`give_up_overdue`]); waiting offline or logged out does not, and a row whose
//! budget ended that way still gets one late attempt when the account is back, also when a
//! wait for the user stands in front of it then. That attempt is its last: answered with a
//! challenge or a safety number, the row fails instead of waiting on
//! ([`record_outcome`]). Where a network names the latest revision in an edit
//! (`EditTarget::LatestRevision`, Signal), the edit names its target when it is sent, not when
//! it was typed: the revision right before its own that is still there (Signal Desktop's
//! `getTargetOfThisEditTimestamp`), so an edit queued behind one the network refused (and the
//! core rolled back) targets what the recipients have. Where it names the original
//! (`EditTarget::Original`, WhatsApp and Threema) the request holds the message's own key from
//! the start and [`take_due`] leaves it alone.
//!
//! Clock changes (§9.6): a wait is stored as a wall-clock time. After the clock was set back, a
//! row would wait for the old time; a wait longer than any wait can be (the network's budget)
//! is read as due, and the dispatcher clears the waits of its account when it sees the clock go back.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kourier_backend_api::{
    AllocatedId, IncomingMessage, Network, OutgoingKind, OutgoingRequest, TransmitOutcome,
};
use kourier_db::failpoint;
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use kourier_model::{
    AccountHold, AccountId, AccountState, AttachmentId, BackendError, BlobKey, BlobRef, Body,
    ConvKey, ConversationFeatures, ConversationId, DisappearKind, EditTarget, ErrorKind, Expiry,
    FailReason, MessageId, MsgIdScope, MsgTarget, NetMsgId, NetworkKind, Origin, OutboxPolicy,
    OutgoingState, QuoteSnapshot, RecipientState, Redacted, ReplyTo, Text, UserId,
};

use crate::actions::{self, Facts, Sending};
use crate::error::{CoreError, Refusal, Result};
use crate::ingest::{Apply, Changes};
use crate::rules::NetRules;
use crate::sql;

/// Kill point after an attempt was recorded (`Transmitting`), before `Transmit` is sent (§10.4).
pub const FAILPOINT_AFTER_ATTEMPT: &str = "outbox_after_attempt_commit";
/// Kill point inside the outcome transaction, before its commit: the outcome is lost and the
/// restart resends with the same id (§10.4).
pub const FAILPOINT_BEFORE_OUTCOME_COMMIT: &str = "outbox_before_outcome_commit";

/// Kill point right after the outcome transaction committed (§10.4): the answer is recorded,
/// and with it a challenge the account waits on from then; the restart finds both.
pub const FAILPOINT_AFTER_OUTCOME: &str = "outbox_after_outcome_commit";

/// Kill point right after the answer to a stop (`Stopping`: the durable mark
/// [`CANCEL_STOPPING`]; `TooLate`: the mark cleared) was committed, before the attempt's outcome
/// is recorded. After `Stopping` the stopped attempt reached nobody and the restart takes the
/// message back; after `TooLate` the restart sends it as it would have (upload progress, critic
/// B2).
pub const FAILPOINT_AFTER_CANCEL_ANSWER: &str = "outbox_after_cancel_answer";

/// Kill point right after the X's mark ([`CANCEL_ASKED`]) was committed, before the backend was
/// asked (upload progress, fix K4). Nothing was promised: the restart clears the mark and the
/// message goes out as it would have.
pub const FAILPOINT_AFTER_CANCEL_ASKED: &str = "outbox_after_cancel_asked";

/// Kill point inside the enqueue transaction, after each message of the send and its outbox
/// row were written, before the commit (§10.4): the kill leaves none of them.
pub const FAILPOINT_IN_ENQUEUE: &str = "outbox_in_enqueue";
/// Kill point right after the enqueue transaction committed, before the dispatcher hears of
/// it (§10.4): every message of the send exists and the restart sends each once.
pub const FAILPOINT_AFTER_ENQUEUE: &str = "outbox_after_enqueue_commit";

/// Retry budget of one message, from enqueue: Signal's one day, the default policy's value
/// ([`OutboxPolicy::SIGNAL`]). A network states its own in its descriptor ([`NetRules::outbox`]);
/// every site of the outbox reads the account's policy, not this constant.
pub const BUDGET: Duration = OutboxPolicy::SIGNAL.budget;
/// Longest wait between two attempts: Signal's, the default policy's value.
pub const MAX_BACKOFF: Duration = OutboxPolicy::SIGNAL.max_backoff;

pub(crate) const QUEUED: &str = "Queued";
pub(crate) const TRANSMITTING: &str = "Transmitting";
pub(crate) const FAILED: &str = "Failed";
/// A row of a large send whose messages are still being written ([`enqueue_files`]): not
/// seen by the dispatcher until the whole send is stored and its rows become `Queued`.
pub(crate) const STAGED: &str = "Staged";
/// `outbox.kind` of a message send; edits, deletes and reactions are rows of their target
/// ([`crate::modify`]).
pub(crate) const MESSAGE: &str = "Message";

/// `outbox.cancel_state` (core.db 17): no stop was asked.
pub(crate) const CANCEL_NONE: i64 = 0;
/// The X was pressed; the backend's answer is not recorded yet. Nothing was promised: an
/// outcome, a lost reply and a restart clear it and the message goes on as it was.
pub(crate) const CANCEL_ASKED: i64 = 1;
/// The backend said `Stopping`: nothing of the message goes out any more. Only this mark takes
/// the message back when the attempt ends `Failed` or without a reply, and at the next start.
pub(crate) const CANCEL_STOPPING: i64 = 2;

/// The wait before the next attempt after `failures` failed attempts: 0 ms, 190 ms, then ×1.9,
/// capped at [`MAX_BACKOFF`] (Signal's `exponentialBackoffSleepTime`,
/// `SD/ts/util/exponentialBackoff.std.ts`). The core's sites use [`backoff_capped`] with the
/// account's cap ([`OutboxPolicy::max_backoff`]); this is the curve with Signal's cap.
pub fn backoff(failures: u32) -> Duration {
    backoff_capped(failures, MAX_BACKOFF)
}

/// [`backoff`] with the cap of a network's policy ([`OutboxPolicy::max_backoff`]). The curve's
/// shape (0, 190 ms, ×1.9) is the core's for every network; only its cap is the network's.
pub fn backoff_capped(failures: u32, cap: Duration) -> Duration {
    let max = cap.as_millis() as f64;
    let ms = match failures {
        0 | 1 => 0.0,
        n => (190.0 * 1.9f64.powi(i32::try_from(n - 2).unwrap_or(i32::MAX))).min(max),
    };
    Duration::from_millis(ms as u64)
}

pub(crate) fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// A new transaction with the [`Apply`] state of `account` (ingest's helpers: send-state
/// aggregation, counters, invalidations), for the paths that know only the scope: they apply
/// nothing that waited. A path that has the network's [`NetRules`] uses [`Apply::under_rules`].
fn apply_for<'c>(
    tx: &'c Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
) -> Result<Apply<'c>> {
    Apply::begin(tx, account, scope, now)
}

/// The account and network of a conversation.
pub(crate) fn network_of(
    conn: &Connection,
    conv: ConversationId,
) -> Result<Option<(AccountId, NetworkKind)>> {
    let row: Option<(i64, String)> = conn
        .prepare_cached(
            "SELECT c.account_id, a.network FROM conversations c JOIN accounts a ON a.id = c.account_id
             WHERE c.id = ?1",
        )?
        .query_row([conv.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(a, n)| Ok((AccountId(a), sql::network(&n)?)))
        .transpose()
}

/// The account and network of a message.
pub(crate) fn message_network(
    conn: &Connection,
    message: MessageId,
) -> Result<Option<(AccountId, NetworkKind)>> {
    let row: Option<(i64, String)> = conn
        .prepare_cached(
            "SELECT m.account_id, a.network FROM messages m JOIN accounts a ON a.id = m.account_id
             WHERE m.id = ?1",
        )?
        .query_row([message.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(a, n)| Ok((AccountId(a), sql::network(&n)?)))
        .transpose()
}

/// What a write of this module changed.
#[derive(Debug)]
pub(crate) struct Written {
    pub account: AccountId,
    pub conv: ConversationId,
    pub changes: Changes,
}

// ---------- enqueue ----------

/// The settings row that keeps the last `now_ms` given to `allocate_message_id`, so ids stay
/// strictly increasing per account across restarts and clock jumps.
pub(crate) fn alloc_scope(account: AccountId) -> String {
    format!("account:{}", account.0)
}

const ALLOC_KEY: &str = "last_alloc_ms";

/// How many taken ids [`enqueue_text`] skips before it gives up.
const MAX_ALLOC_TRIES: u32 = 1000;

/// Allocates the network id of a new own send in `conv` (a message, or an edit, delete or
/// reaction, which are network messages of their own): strictly increasing per account (the
/// watermark survives restarts and clock jumps), and never the key of an existing own message or
/// revision, or an id a message had before the server named it ([`Apply::by_key`]).
pub(crate) fn allocate(
    apply: &Apply<'_>,
    network: &dyn Network,
    conv: ConversationId,
    key: &ConvKey,
    now: i64,
) -> Result<AllocatedId> {
    let account = apply.acc.id;
    let last: i64 = apply
        .conn
        .prepare_cached("SELECT value FROM settings WHERE scope = ?1 AND key = ?2")?
        .query_row((alloc_scope(account), ALLOC_KEY), |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(i64::MIN);
    let mut at = now.max(last.saturating_add(1));
    let mut tries = 0;
    let id = loop {
        let id = network.allocate_message_id(
            &apply.acc.own_ids,
            key,
            u64::try_from(at.max(0)).unwrap_or(0),
        );
        let revision: bool = apply
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM message_revisions WHERE account_id = ?1
                   AND net_author = '' AND net_local = ?2)",
            )?
            .query_row((account.0, id.local.as_ref()), |r| r.get(0))?;
        if !revision && apply.by_key(conv, "", &id.local)?.is_none() {
            break id;
        }
        // A network whose ids do not follow `now_ms` must not spin the writer thread.
        tries += 1;
        if tries >= MAX_ALLOC_TRIES {
            return Err(CoreError::InvalidState(
                "the network allocated only ids that are taken",
            ));
        }
        at = at.saturating_add(1);
    };
    apply
        .conn
        .prepare_cached(
            "INSERT INTO settings(scope, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        )?
        .execute((alloc_scope(account), ALLOC_KEY, at.to_string()))?;
    Ok(id)
}

/// Allocates a message id for a roaming change of `conv` that a network carries as a message
/// of its own (a timer change): from the same allocator as own messages, in its own
/// transaction, so the id is never that of another own message.
pub(crate) fn allocate_for_change(
    conn: &mut Connection,
    network: &dyn Network,
    conv: ConversationId,
    now: i64,
) -> Result<AllocatedId> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(i64, String)> = tx
        .prepare_cached("SELECT account_id, conv_key FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (account, key) = row.ok_or(CoreError::UnknownConversation(conv))?;
    let apply = apply_for(
        &tx,
        AccountId(account),
        network.descriptor().msg_id_scope,
        now,
    )?;
    let id = allocate(&apply, network, conv, &sql::conv_key(&key)?, now)?;
    drop(apply);
    tx.commit()?;
    Ok(id)
}

/// The quote of an existing message of `conv`, checked by [`actions::reply`] (the rule behind
/// a row's `reply` flag): as the message row stores it, and as the request carries it (with the
/// thumbnail blob of the quoted attachment, for networks that send one with a quote).
fn reply_to(
    apply: &Apply<'_>,
    conv: ConversationId,
    key: &ConvKey,
    blocked: bool,
    features: Option<&ConversationFeatures>,
    id: MessageId,
) -> Result<(ReplyTo, ReplyTo)> {
    let conn = apply.conn;
    type Row = (String, String, String, String, Option<String>, i64, i64);
    let row: Option<Row> = conn
        .prepare_cached(
            "SELECT net_author, net_local, body, body_kind, deleted, sent_at, revision
             FROM messages WHERE id = ?1 AND conversation_id = ?2 AND expiry_hidden = 0",
        )?
        .query_row((id.0, conv.0), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .optional()?;
    let (author, local, body, body_kind, deleted, sent_at, revision) =
        row.ok_or(CoreError::refused(
            Refusal::Gone,
            "the quoted message is not in this conversation",
        ))?;
    let body: Body = sql::from_json(&body)?;
    actions::reply(&Facts {
        active: apply.acc.state == AccountState::Active,
        registered: true,
        blocked,
        features,
        admin: false,
        own: author.is_empty(),
        body_kind: &body_kind,
        deleted: deleted.as_deref(),
        sent_at,
        revision: u16::try_from(revision.max(0)).unwrap_or(u16::MAX),
        media: None,
        has_text: false,
        sending: Sending::Settled,
        note_to_self: false,
        queued: false,
    })?;
    let author = (!author.is_empty())
        .then(|| sql::user(&author))
        .transpose()?;
    let net_id = NetMsgId {
        author: author.clone(),
        local: local.as_str().into(),
    };
    let (text, first) = match body {
        Body::Text(t) => (Some(t), None),
        Body::Media {
            attachments,
            caption,
        } => (
            caption,
            attachments
                .into_iter()
                .enumerate()
                .find(|(_, a)| !a.long_text),
        ),
        Body::Sticker(a) => (None, Some((0, *a))),
        // A card is quoted as Signal Android quotes it (`Contact.quoteText`): "👤 " and the
        // card's name; never the number (SD5).
        Body::Contact(card) => (
            card.name
                .as_deref()
                .map(|name| Text::plain(format!("\u{1F464} {name}"))),
            None,
        ),
        _ => (None, None),
    };
    let thumb = match &first {
        Some((idx, _)) => {
            let blob: Option<(String, Vec<u8>)> = conn
                .prepare_cached(
                    "SELECT f.path, f.key FROM attachments a JOIN files f ON f.id = a.thumb_file_id
                     WHERE a.message_id = ?1 AND a.idx = ?2",
                )?
                .query_row((id.0, i64::try_from(*idx).unwrap_or(i64::MAX)), |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            blob.and_then(|(path, key)| {
                Some(BlobRef {
                    path: path.into(),
                    key: BlobKey::new(key.try_into().ok()?),
                })
            })
        }
        None => None,
    };
    let stored = ReplyTo {
        target: MsgTarget::to_id(Some(key.clone()), &net_id),
        author,
        snapshot: Some(QuoteSnapshot {
            text,
            attachment: first.as_ref().map(|(_, a)| a.kind),
            file_name: first.as_ref().and_then(|(_, a)| a.file_name.clone()),
            mime: first
                .as_ref()
                .map(|(_, a)| a.mime.clone())
                .filter(|m| !m.is_empty()),
            thumb: None,
        }),
    };
    let mut request = stored.clone();
    if let Some(snapshot) = request.snapshot.as_mut() {
        snapshot.thumb = thumb.map(Box::new);
    }
    Ok((stored, request))
}

/// §4.8 step 1: one transaction inserts the message row (`Queued`, own, no author), allocates
/// its network id with [`Network::allocate_message_id`] and inserts the outbox row with the
/// request. Clears the conversation's draft. A conversation that cannot take messages
/// (`ConversationFeatures::cannot_send`) is refused before anything is stored, the draft
/// stays ([`Target::read`] is the gate of every new send).
///
/// A text longer than one message of the conversation carries
/// (`ConversationFeatures::max_text_bytes`: on Signal the size of a long-text attachment,
/// which its clients cut off at on receipt, `SD/ts/jobs/AttachmentDownloadManager.preload.ts`)
/// is not refused: it becomes as many messages as it needs ([`Text::split`]), stored in the
/// same transaction (all or none) and sent in order, the first carrying the quote.
pub(crate) fn enqueue_text(
    conn: &mut Connection,
    network: &dyn Network,
    conv: ConversationId,
    text: Text,
    reply: Option<MessageId>,
    now: i64,
) -> Result<Written> {
    let stored = enqueue(conn, network, conv, reply, now, true, |features| {
        let max = features
            .and_then(|f| f.max_text_bytes)
            .and_then(|max| usize::try_from(max).ok());
        let pieces = match max {
            Some(max) => text.split(max),
            None => vec![text.clone()],
        };
        Ok(pieces
            .into_iter()
            .enumerate()
            .map(|(i, piece)| Outgoing {
                body: Body::Text(piece),
                quotes: i == 0,
                files: Vec::new(),
            })
            .collect())
    })?;
    Ok(stored.written)
}

/// §4.8 step 1 for a contact card (P5c, `UiCommand::SendContact`): ONE message whose body is
/// `card` (built by the core from the contact row, [`crate::facade::PersonDetail::contact_card`]),
/// queued through the conversation's FIFO like a text. The chat's draft is NOT cleared (the
/// card was not typed there), and a chat that cannot send refuses it (the gate of
/// [`Target::read`], as for a text: the composer is not there to stop it).
pub(crate) fn enqueue_contact(
    conn: &mut Connection,
    network: &dyn Network,
    conv: ConversationId,
    card: kourier_model::ContactCard,
    now: i64,
) -> Result<Written> {
    let stored = enqueue(conn, network, conv, None, now, false, |_| {
        Ok(vec![Outgoing {
            body: Body::Contact(card),
            quotes: false,
            files: Vec::new(),
        }])
    })?;
    Ok(stored.written)
}

/// Why [`enqueue_files`] stored nothing.
#[derive(Debug)]
pub(crate) enum FilesError {
    /// The conversation does not take these files (by position in the selection).
    Refused(Vec<crate::facade::RefusedFile>),
    /// Anything else.
    Other(CoreError),
}

impl From<CoreError> for FilesError {
    fn from(e: CoreError) -> Self {
        Self::Other(e)
    }
}

/// Kill point behind the write that forgets a challenge the network accepted the answer to,
/// before the account's pause is lifted (§10.4): the next start shows no challenge.
pub const FAILPOINT_AFTER_CHALLENGE_ANSWER: &str = "outbox_after_challenge_answer";

/// Most messages one enqueue transaction writes. A send that becomes more is written in
/// several transactions ([`enqueue_files`]): the one writer thread serves every account, and
/// a few thousand message inserts in one transaction would hold all of them for seconds
/// (measured in a release build on this machine's busy disk: 0.5 to 0.8 ms per message of
/// a send, 2.4 to 4.1 s for 5,000; a transaction of this size takes 30 to 50 ms, which is
/// then the longest another chat's write waits). Pacing only: nothing is refused by it.
pub const ENQUEUE_CHUNK: usize = 64;

/// Kill point after a transaction that stored a part of a large send as `Staged` rows, before
/// the next part (§10.4): the next start removes what was stored of the send.
pub const FAILPOINT_AFTER_STAGED: &str = "outbox_after_staged_commit";

/// A large send between two of its transactions: what is still to be written.
pub(crate) struct FilesSend {
    conv: ConversationId,
    files: std::sync::Arc<[crate::attachments::import::Imported]>,
    /// The messages not stored yet, in sending order.
    rest: std::collections::VecDeque<Outgoing>,
    /// The messages stored so far, in sending order.
    messages: Vec<MessageId>,
    changes: Changes,
    clear_draft: bool,
}

impl FilesSend {
    /// The messages stored so far, in sending order: what [`discard_staged`] drops when a
    /// later part fails.
    pub(crate) fn stored(&self) -> &[MessageId] {
        &self.messages
    }
}

impl std::fmt::Debug for FilesSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesSend")
            .field("conv", &self.conv)
            .field("stored", &self.messages.len())
            .field("rest", &self.rest.len())
            .finish_non_exhaustive()
    }
}

/// How far [`enqueue_files`] or [`enqueue_files_more`] got.
#[derive(Debug)]
pub(crate) enum FilesStep {
    /// A part is stored (`Staged`); call [`enqueue_files_more`] with this.
    More(Box<FilesSend>),
    /// The whole send is stored and queued: what changed, and its messages in sending order.
    Done(Box<Written>, Vec<MessageId>),
}

/// §4.8 step 1 for files (§5.8): the selection becomes as many messages as the conversation's
/// packing rule needs (`ConversationFeatures::plan`: neighbours that may share a message do,
/// by kind and by format; every other file travels alone; the user's order is kept), each a
/// `Media` body with its own network id and outbox row. They are sent in this order through
/// the conversation's FIFO. The attachment rows point at the imported blobs (`files` rows
/// for each file and thumbnail, state `Local`); the outbox requests carry the blob
/// references the backend reads. The caption and the quote (`reply`, a message of the
/// conversation, as for a text) go on the first message; where that message takes no
/// caption, the caption goes first as a text of its own and carries the quote. So does a
/// caption longer than one message of the conversation carries
/// (`ConversationFeatures::max_text_bytes`): it goes first as the texts it is split into
/// ([`Text::split`]), the quote on the first of them, and the files follow without a
/// caption. The draft is cleared only when a caption was sent. A conversation that cannot
/// take messages (`ConversationFeatures::cannot_send`) is refused with
/// [`Refusal::CannotSend`] before the files are checked against its features and before
/// anything is stored: this first part is the gate of the whole send; the parts after it are
/// not gated again.
///
/// All of them or none, also across a kill. A send of up to [`ENQUEUE_CHUNK`] messages is
/// one transaction. A larger one is written in parts so the writer thread is free in
/// between: every part is a transaction of its own whose outbox rows are `Staged`, which
/// the dispatcher does not see, and the last transaction turns them all into `Queued`. Until
/// then the send can still be dropped: by [`discard_staged`] when a later part fails, and by
/// [`discard_all_staged`] at the next start after a kill. A conversation that was hidden
/// from the list (deleted, empty) is listed again by that last transaction, not by a part:
/// a dropped send leaves it as it was. The caller runs the parts one
/// after another and nothing else of the conversation in between (the compose queue of the
/// conversation). A send knows its rows by their messages, not by the conversation: a merge
/// of two chats of one person (§3.3) between two parts moves the stored messages and their
/// outbox rows into the chat that stays, the send goes on there
/// ([`enqueue_files_more`]), and the `Staged` rows of another send that was being stored in
/// that chat are not touched by this one.
pub(crate) fn enqueue_files(
    conn: &mut Connection,
    network: &dyn Network,
    conv: ConversationId,
    files: std::sync::Arc<[crate::attachments::import::Imported]>,
    caption: Option<Text>,
    reply: Option<MessageId>,
    now: i64,
) -> std::result::Result<FilesStep, FilesError> {
    use crate::attachments::import::check_features;
    if files.is_empty() {
        return Err(CoreError::InvalidInput("no files to send").into());
    }
    let caption = caption.filter(|t| !t.body.trim().is_empty());
    let clear_draft = caption.is_some();
    let mut refused = Vec::new();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(CoreError::from)?;
    let stored = (|| {
        let mut target = Target::read(&tx, network, conv, Part::First, now)?;
        let features = target.features.as_ref();
        // The kinds are those of what was imported: the content decides, at import time.
        for (index, f) in files.iter().enumerate() {
            if let Err(reason) = check_features(features, f.kind, f.file.size) {
                refused.push(crate::facade::RefusedFile { index, reason });
            }
        }
        if !refused.is_empty() {
            return Err(crate::attachments::import::refusal_error(&refused));
        }
        let mut rest: std::collections::VecDeque<Outgoing> =
            plan_files(features, &files, caption, reply.is_some())?.into();
        let mut quote = target.quote(conv, reply)?;
        let whole = rest.len() <= ENQUEUE_CHUNK;
        let state = if whole { QUEUED } else { STAGED };
        let was_hidden = is_hidden(&tx, conv)?;
        let messages = target.store(
            network,
            conv,
            &mut rest,
            ENQUEUE_CHUNK,
            &mut quote,
            state,
            &files,
        )?;
        if !whole && was_hidden {
            hide_again(&tx, conv)?;
        }
        if whole && clear_draft {
            tx.prepare_cached("UPDATE conversations SET draft = NULL WHERE id = ?1")?
                .execute([conv.0])?;
        }
        let (account, changes) = target.finish()?;
        Ok((account, changes, messages, rest))
    })();
    let (account, changes, messages, rest) = match stored {
        Ok(s) => s,
        Err(_) if !refused.is_empty() => return Err(FilesError::Refused(refused)),
        Err(e) => return Err(e.into()),
    };
    tx.commit().map_err(CoreError::from)?;
    if rest.is_empty() {
        failpoint::hit(FAILPOINT_AFTER_ENQUEUE);
        return Ok(FilesStep::Done(
            Box::new(Written {
                account,
                conv,
                changes,
            }),
            messages,
        ));
    }
    failpoint::hit(FAILPOINT_AFTER_STAGED);
    Ok(FilesStep::More(Box::new(FilesSend {
        conv,
        files,
        rest,
        messages,
        changes,
        clear_draft,
    })))
}

/// The next part of a large send ([`enqueue_files`]): up to [`ENQUEUE_CHUNK`] more messages
/// as `Staged` rows; with the last of them, every `Staged` row of the send becomes `Queued`
/// and the draft is cleared, in the same transaction.
///
/// The part is written where the parts before it are now: when their conversation was
/// merged into another one meanwhile, that is the conversation that stayed. It is not
/// refused for `cannot_send`: the first part passed the gate, and a conversation that lost
/// the right since still gets the rest (the send is all of it or none; the network fails
/// what it will not take, message by message).
pub(crate) fn enqueue_files_more(
    conn: &mut Connection,
    network: &dyn Network,
    mut send: Box<FilesSend>,
    now: i64,
) -> Result<FilesStep> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let moved: Option<i64> = match send.messages.first() {
        Some(first) => tx
            .prepare_cached("SELECT conversation_id FROM messages WHERE id = ?1")?
            .query_row([first.0], |r| r.get(0))
            .optional()?,
        None => None,
    };
    if let Some(now_in) = moved {
        send.conv = ConversationId(now_in);
    }
    let conv = send.conv;
    // Not the first part: a send the chat took is finished even if it lost the right since.
    let mut target = Target::read(&tx, network, conv, Part::Next, now)?;
    let files = send.files.clone();
    let was_hidden = is_hidden(&tx, conv)?;
    // The quote went with the first part.
    let messages = target.store(
        network,
        conv,
        &mut send.rest,
        ENQUEUE_CHUNK,
        &mut None,
        STAGED,
        &files,
    )?;
    send.messages.extend(messages);
    let done = send.rest.is_empty();
    if !done && was_hidden {
        hide_again(&tx, conv)?;
    }
    if done {
        // Its own rows, by message (`outbox_by_message`): another send that is being
        // stored in this conversation after a merge keeps its rows `Staged`.
        // A network that refused one of this send's own messages as a first contact while the
        // send was being stored (a 463 landed between two parts: `fail_waiting_of` failed the
        // rows already stored) has refused the rest too: those parts were typed before, and
        // each is one more reach-out. They fail without an attempt instead of turning due
        // (K-A fix 2). A refusal of an earlier message of the chat does not count: this send
        // was begun after it, and a message typed afterwards is the user's own new act.
        if reach_out_refused_in(&tx, &send.messages)? {
            fail_staged_of(&tx, &mut target.apply, conv, &send.messages)?;
        } else {
            let mut queue = tx.prepare_cached(
                "UPDATE outbox SET state = ?2 WHERE message_id = ?1 AND kind = 'Message'
                   AND state = ?3",
            )?;
            for message in &send.messages {
                queue.execute((message.0, QUEUED, STAGED))?;
            }
        }
        if send.clear_draft {
            tx.prepare_cached("UPDATE conversations SET draft = NULL WHERE id = ?1")?
                .execute([conv.0])?;
        }
    }
    let (account, changes) = target.finish()?;
    send.changes.absorb(changes);
    tx.commit()?;
    if !done {
        failpoint::hit(FAILPOINT_AFTER_STAGED);
        return Ok(FilesStep::More(send));
    }
    failpoint::hit(FAILPOINT_AFTER_ENQUEUE);
    let FilesSend {
        messages, changes, ..
    } = *send;
    Ok(FilesStep::Done(
        Box::new(Written {
            account,
            conv,
            changes,
        }),
        messages,
    ))
}

/// Whether `conv` is hidden from the list (it was deleted and nothing was stored or queued
/// in it since, `conversations.hidden`).
fn is_hidden(conn: &Connection, conv: ConversationId) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT hidden FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}

/// A part of a large send was stored as `Staged` rows in a conversation that was hidden
/// (deleted, empty): storing a message lists a conversation again, but this send is not
/// there yet. It can still be dropped as a whole ([`discard_staged`], [`discard_all_staged`]
/// after a kill), and a dropped send must not leave an empty chat in the list. So the
/// conversation stays hidden until the transaction that queues the send, which stores its
/// last messages and lists it by that.
fn hide_again(conn: &Connection, conv: ConversationId) -> Result<()> {
    conn.prepare_cached("UPDATE conversations SET hidden = 1 WHERE id = ?1")?
        .execute([conv.0])?;
    Ok(())
}

/// The messages of a selection of imported `files`, in sending order ([`enqueue_files`]).
fn plan_files(
    features: Option<&ConversationFeatures>,
    files: &[crate::attachments::import::Imported],
    caption: Option<Text>,
    reply: bool,
) -> Result<Vec<Outgoing>> {
    let of: Vec<_> = files.iter().map(|f| f.plan_file()).collect();
    // The plan the composer was shown: the same function on the same facts (kind and
    // format of each file, the caption itself).
    let plan = crate::attachments::plan_files(features, &of, caption.as_ref(), reply);
    if plan.reply_unsendable {
        return Err(CoreError::refused(
            Refusal::NotAllowedHere,
            "this chat cannot quote a message with files and no text",
        ));
    }
    let mut out = Vec::with_capacity(plan.message_count());
    let mut caption = caption;
    // A caption the first message with files cannot carry (its kind takes none, files
    // cannot quote here, or it is longer than one message of the chat carries, which
    // the backend would refuse whole) goes first, as the texts it is split into, like
    // a text of that length ([`enqueue_text`]).
    let max = features
        .and_then(|f| f.max_text_bytes)
        .and_then(|max| usize::try_from(max).ok());
    if plan.lead_text
        && let Some(text) = caption.take()
    {
        let pieces = match max {
            Some(max) => text.split(max),
            None => vec![text],
        };
        for piece in pieces {
            out.push(Outgoing {
                body: Body::Text(piece),
                // The first message of the send carries the quote.
                quotes: out.is_empty(),
                files: Vec::new(),
            });
        }
    }
    for group in plan.messages {
        let first = out.is_empty();
        // Only the first message of the send carries the caption.
        let caption = caption.take();
        let attachments = group
            .iter()
            .filter_map(|i| files.get(*i))
            .map(|f| f.attachment(false))
            .collect();
        out.push(Outgoing {
            body: Body::Media {
                attachments,
                caption,
            },
            quotes: first,
            files: group,
        });
    }
    Ok(out)
}

/// Drops what a large send stored of itself before it failed ([`enqueue_files`]): those of
/// `messages` (the send's, [`FilesSend::stored`]) whose own send is still `Staged`, with
/// their attachment rows and the `files` rows of their blobs (the files are unlinked after
/// the commit). Nothing of them was sent, and nobody was told. The rows are found by their
/// messages, so they are found wherever a merge of two conversations took them.
pub(crate) fn discard_staged(
    conn: &mut Connection,
    scope: MsgIdScope,
    messages: &[MessageId],
    now: i64,
) -> Result<Changes> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // (message, conversation, account) of what is still staged.
    let mut staged: Vec<(MessageId, ConversationId, AccountId)> = Vec::new();
    {
        let mut find = tx.prepare_cached(
            "SELECT m.conversation_id, m.account_id FROM outbox o
               JOIN messages m ON m.id = o.message_id
             WHERE o.message_id = ?1 AND o.kind = 'Message' AND o.state = ?2",
        )?;
        for id in messages {
            let row: Option<(i64, i64)> = find
                .query_row((id.0, STAGED), |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            if let Some((conv, account)) = row {
                staged.push((*id, ConversationId(conv), AccountId(account)));
            }
        }
    }
    let Some(&(_, _, account)) = staged.first() else {
        return Ok(Changes::default());
    };
    let mut apply = apply_for(&tx, account, scope, now)?;
    let mut convs = BTreeSet::new();
    for (id, conv, _) in &staged {
        apply.delete_attachments(*id)?;
        tx.prepare_cached("DELETE FROM messages WHERE id = ?1")?
            .execute([id.0])?;
        convs.insert(*conv);
    }
    for conv in convs {
        apply.recount(conv)?;
        apply.ch.conversations.insert(conv);
        apply.ch.timelines.insert(conv);
    }
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(changes)
}

/// At a start: no send is being stored any more, so every `Staged` row is what a kill left
/// of a large send ([`enqueue_files`]). Their messages go, with their attachment and `files`
/// rows; the blobs are unreferenced then and the startup sweep removes them. Returns how
/// many messages went.
pub(crate) fn discard_all_staged(conn: &mut Connection) -> Result<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let staged: Vec<(i64, i64)> = tx
        .prepare_cached(
            "SELECT o.message_id, o.conversation_id FROM outbox o
             WHERE o.state = ?1 AND o.kind = 'Message' ORDER BY o.id",
        )?
        .query_map([STAGED], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    for (message, _) in &staged {
        // The attachment rows cascade; the `files` rows they named do not.
        tx.prepare_cached(
            "DELETE FROM files WHERE id IN (
               SELECT file_id FROM attachments WHERE message_id = ?1 AND file_id IS NOT NULL
               UNION SELECT thumb_file_id FROM attachments
                 WHERE message_id = ?1 AND thumb_file_id IS NOT NULL)",
        )?
        .execute([message])?;
        tx.prepare_cached("DELETE FROM messages WHERE id = ?1")?
            .execute([message])?;
    }
    let convs: BTreeSet<i64> = staged.iter().map(|(_, c)| *c).collect();
    for conv in convs {
        // `last_activity` from the rows that are left.
        crate::ingest::recount(&tx, ConversationId(conv))?;
    }
    tx.commit()?;
    Ok(staged.len())
}

/// What one [`enqueue`] stored.
struct Enqueued {
    written: Written,
}

/// One message a send stores and sends.
struct Outgoing {
    /// The body stored in `messages.body`. The body of the outbox request is the same, with
    /// the blob references of its files.
    body: Body,
    /// This message carries the send's quote, when there is one.
    quotes: bool,
    /// The files of the selection this message carries, in order (positions).
    files: Vec<usize>,
}

/// The conversation of a send inside its transaction: read, checked, and the ingest state
/// its messages are stored through.
struct Target<'c> {
    apply: Apply<'c>,
    /// The network's rules: the retry budget a stored row gets ([`Target::store`]).
    rules: NetRules,
    account: AccountId,
    key: ConvKey,
    features: Option<ConversationFeatures>,
    timer: Option<Duration>,
}

/// Which part of a send a transaction stores: [`Target::read`] gates only the first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    /// The first transaction of a NEW send (a text, a card, the first messages of some
    /// files): the chat must take messages (`ConversationFeatures::cannot_send`).
    First,
    /// A further part of a send that was accepted ([`enqueue_files_more`]): never refused for
    /// the right to send. The send is "all of it or none" (core.md): a chat that lost the right
    /// between two parts must not leave a half of a send behind, and the parts are one act of
    /// the user, whose first part the chat did take.
    Next,
}

impl<'c> Target<'c> {
    /// Reads `conv` and checks that its account is linked and it is not blocked; for the
    /// [`Part::First`] of a send it also checks that the chat lets us send. THE GATE of every
    /// new send (G4): the features are read in the same transaction that stores the messages
    /// and clears the draft, so a refusal stores nothing and keeps the draft, and a chat that
    /// lost the right a moment after the composer was shown (a stale UI, a second front-end, a
    /// race) never gets a message the network would only fail.
    fn read(
        tx: &'c Connection,
        network: &dyn Network,
        conv: ConversationId,
        part: Part,
        now: i64,
    ) -> Result<Self> {
        // (account, key, blocked, timer, features)
        type Conv = (i64, String, bool, Option<i64>, Option<String>);
        let row: Option<Conv> = tx
            .prepare_cached(
                "SELECT account_id, conv_key, blocked, timer_secs, features FROM conversations
                 WHERE id = ?1",
            )?
            .query_row([conv.0], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let (account, key, blocked, timer, features) =
            row.ok_or(CoreError::UnknownConversation(conv))?;
        let account = AccountId(account);
        let rules = NetRules::of(network.descriptor());
        let apply = Apply::under_rules(tx, account, rules, now)?;
        if apply.acc.state != AccountState::Active {
            return Err(CoreError::InvalidState("the account is not linked"));
        }
        if blocked {
            return Err(CoreError::refused(
                Refusal::Blocked,
                "the conversation is blocked",
            ));
        }
        let features: Option<ConversationFeatures> = sql::from_json_opt(features)?;
        // NULL features are "sendable", as `ConversationRow.can_send` says (core.md).
        if part == Part::First && features.as_ref().is_some_and(|f| f.cannot_send.is_some()) {
            return Err(CoreError::refused(
                Refusal::CannotSend,
                "the conversation does not take messages",
            ));
        }
        Ok(Self {
            apply,
            rules,
            account,
            key: sql::conv_key(&key)?,
            features,
            timer: timer
                .filter(|s| *s > 0)
                .map(|s| Duration::from_secs(u64::try_from(s).unwrap_or(0))),
        })
    }

    /// The quote of the send, resolved once, for the message that carries it.
    fn quote(
        &self,
        conv: ConversationId,
        reply: Option<MessageId>,
    ) -> Result<Option<(ReplyTo, ReplyTo)>> {
        reply
            .map(|id| {
                reply_to(
                    &self.apply,
                    conv,
                    &self.key,
                    false,
                    self.features.as_ref(),
                    id,
                )
            })
            .transpose()
    }

    /// Stores up to `most` messages from the front of `items`, in order: each a message row
    /// with its own network id, its files' rows, and an outbox row in `state`. Returns
    /// their ids.
    #[allow(clippy::too_many_arguments)]
    fn store(
        &mut self,
        network: &dyn Network,
        conv: ConversationId,
        items: &mut std::collections::VecDeque<Outgoing>,
        most: usize,
        quote: &mut Option<(ReplyTo, ReplyTo)>,
        state: &str,
        files: &[crate::attachments::import::Imported],
    ) -> Result<Vec<MessageId>> {
        let (tx, now) = (self.apply.conn, self.apply.now);
        let mut messages = Vec::with_capacity(most.min(items.len()));
        for _ in 0..most {
            let Some(mut out) = items.pop_front() else {
                break;
            };
            out.body.sanitize_ranges();
            if matches!(&out.body, Body::Text(t) if t.body.trim().is_empty()) {
                return Err(CoreError::refused(Refusal::Empty, "the message is empty"));
            }
            let (reply_to, request_reply) = match quote.take_if(|_| out.quotes) {
                Some((stored, request)) => (Some(stored), Some(request)),
                None => (None, None),
            };
            // Strictly increasing per account, so the messages of one send keep their order
            // on every client that sorts by the id's time.
            let id = allocate(&self.apply, network, conv, &self.key, now)?;
            // The names the network will have for the files of this message, as far as they
            // follow from its id: stored with the message, so a delete of one attachment
            // made on another own device finds it here (`AttachmentDeleted`), and handed to
            // the backend with the request.
            if let Body::Media { attachments, .. } = &mut out.body {
                for (index, a) in attachments.iter_mut().enumerate() {
                    if a.net_ids.is_empty() {
                        a.net_ids = network.attachment_net_ids(&id.local, index);
                    }
                }
            }
            // The request's body: the stored one with the blobs the backend reads.
            let mut request_body = out.body.clone();
            if let Body::Media { attachments, .. } = &mut request_body {
                let own = out.files.iter().filter_map(|i| files.get(*i));
                for (a, f) in attachments.iter_mut().zip(own) {
                    let with_files = f.attachment(true);
                    a.local = with_files.local;
                    a.thumb = with_files.thumb;
                }
            }
            let mut m = IncomingMessage::new(
                self.key.clone(),
                id.local.clone(),
                Origin::OwnThisDevice,
                out.body,
                id.sent_at,
                now,
            );
            m.reply_to = reply_to;
            m.expiry = self.timer.map(|timer| Expiry {
                kind: DisappearKind::AfterSend,
                timer,
                started_at: None,
            });
            m.initial_out_state = Some(OutgoingState::Queued);
            self.apply.message(m, false)?;
            let message = self
                .apply
                .by_key(conv, "", &id.local)?
                .ok_or(CoreError::InvalidState("the queued message was not stored"))?
                .id;
            // The timer starts once a recipient has the message (Signal Desktop,
            // `SD/ts/messages/send.preload.ts`), not at the enqueue: a message queued while
            // offline must not disappear before it was sent (`record_outcome` starts it).
            tx.prepare_cached(
                "UPDATE messages SET expiry_started_at = NULL, expires_at = NULL
                 WHERE id = ?1 AND expiry_timer_ms IS NOT NULL",
            )?
            .execute([message.0])?;
            self.files_of(message, &out.files, files)?;
            let request = OutgoingRequest {
                conv: self.key.clone(),
                local: id.local,
                sent_at: id.sent_at,
                kind: OutgoingKind::Message {
                    body: request_body,
                    reply_to: request_reply,
                    expiry: self.timer,
                },
                only: None,
                is_recipient_update: false,
            };
            tx.prepare_cached(
                "INSERT INTO outbox(message_id, conversation_id, kind, request, state, attempts,
                   next_attempt_at, deadline_at)
                 VALUES (?1, ?2, 'Message', ?3, ?4, 0, NULL, ?5)",
            )?
            .execute((
                message.0,
                conv.0,
                sql::to_json(&request)?,
                state,
                now.saturating_add(ms(self.rules.outbox.budget)),
            ))?;
            self.apply.ch.message(conv, message);
            messages.push(message);
            failpoint::hit(FAILPOINT_IN_ENQUEUE);
        }
        Ok(messages)
    }

    /// The `files` rows of the blobs of `message` (the selection's files at `positions`, in
    /// the order of its attachment rows).
    fn files_of(
        &self,
        message: MessageId,
        positions: &[usize],
        files: &[crate::attachments::import::Imported],
    ) -> Result<()> {
        let (conn, now) = (self.apply.conn, self.apply.now);
        let insert = |w: &crate::attachments::import::Written, purpose: &str| {
            conn.prepare_cached(
                "INSERT INTO files(account_id, path, key, size, mime, purpose)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(kourier_db::rusqlite::params![
                self.account.0,
                w.path.to_string_lossy(),
                &w.key.as_bytes()[..],
                sql::i64_of(w.size),
                w.mime,
                purpose
            ])?;
            Ok::<_, CoreError>(conn.last_insert_rowid())
        };
        let own = positions.iter().filter_map(|i| files.get(*i));
        for (idx, f) in own.enumerate() {
            let file = insert(&f.file, "attachment")?;
            let thumb = f.thumb.as_ref().map(|t| insert(t, "thumb")).transpose()?;
            conn.prepare_cached(
                "UPDATE attachments SET file_id = ?3, thumb_file_id = ?4, derived_at = ?5
                 WHERE message_id = ?1 AND idx = ?2",
            )?
            .execute((
                message.0,
                i64::try_from(idx).unwrap_or(i64::MAX),
                file,
                thumb,
                now,
            ))?;
        }
        Ok(())
    }

    /// Recomputes what the stored messages changed; the caller commits.
    fn finish(mut self) -> Result<(AccountId, Changes)> {
        self.apply.finish()?;
        Ok((self.account, std::mem::take(&mut self.apply.ch)))
    }
}

/// One enqueue transaction of a text send: the conversation is read and checked, `plan` says
/// which messages the send becomes (from the conversation's features as they are inside the
/// transaction), and each is stored as a message row with its own network id and outbox
/// row, in order. All of them or none: an error anywhere drops the transaction.
fn enqueue(
    conn: &mut Connection,
    network: &dyn Network,
    conv: ConversationId,
    reply: Option<MessageId>,
    now: i64,
    clear_draft: bool,
    plan: impl FnOnce(Option<&ConversationFeatures>) -> Result<Vec<Outgoing>>,
) -> Result<Enqueued> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut target = Target::read(&tx, network, conv, Part::First, now)?;
    let mut items: std::collections::VecDeque<Outgoing> = plan(target.features.as_ref())?.into();
    let mut quote = target.quote(conv, reply)?;
    target.store(
        network,
        conv,
        &mut items,
        usize::MAX,
        &mut quote,
        QUEUED,
        &[],
    )?;
    if clear_draft {
        tx.prepare_cached("UPDATE conversations SET draft = NULL WHERE id = ?1")?
            .execute([conv.0])?;
    }
    let (account, changes) = target.finish()?;
    tx.commit()?;
    failpoint::hit(FAILPOINT_AFTER_ENQUEUE);
    Ok(Enqueued {
        written: Written {
            account,
            conv,
            changes,
        },
    })
}

// ---------- recipients and send_state ----------

/// The recipients of `conv` (direct peer, or the members), without our own ids.
fn recipients(apply: &Apply<'_>, conv: ConversationId) -> Result<Vec<UserId>> {
    let key: String = apply
        .conn
        .prepare_cached("SELECT conv_key FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| r.get(0))?;
    if let ConvKey::Direct(peer) = sql::conv_key(&key)? {
        return Ok(if apply.is_own(&peer) {
            Vec::new()
        } else {
            vec![peer]
        });
    }
    let users: Vec<String> = apply
        .conn
        .prepare_cached("SELECT user_id FROM members WHERE conversation_id = ?1 ORDER BY rowid")?
        .query_map([conv.0], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for u in users {
        let u = sql::user(&u)?;
        if !apply.is_own(&u) {
            out.push(u);
        }
    }
    Ok(out)
}

/// `send_state` of one message: user → state.
pub(crate) fn send_states(
    conn: &Connection,
    message: MessageId,
) -> Result<BTreeMap<String, RecipientState>> {
    let rows: Vec<(String, String)> = conn
        .prepare_cached("SELECT user_id, state FROM send_state WHERE message_id = ?1")?
        .query_map([message.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(u, s)| Ok((u, sql::from_json(&s)?)))
        .collect()
}

/// Writes one recipient's outcome. The outbox owns the send state of its own attempts, so this
/// is not ingest's forward-only merge: a failure may follow a failure of another kind, and a
/// retry resets failures to `Pending`; only `Sent` is final.
fn set_recipient(
    apply: &mut Apply<'_>,
    message: MessageId,
    user: &UserId,
    state: RecipientState,
    error: Option<&str>,
) -> Result<()> {
    let col = apply.author_col(Some(user))?;
    if col.is_empty() {
        return Ok(());
    }
    let current: Option<String> = apply
        .conn
        .prepare_cached("SELECT state FROM send_state WHERE message_id = ?1 AND user_id = ?2")?
        .query_row((message.0, col.as_str()), |r| r.get(0))
        .optional()?;
    if let Some(c) = current {
        let c: RecipientState = sql::from_json(&c)?;
        let keep = match (c, state) {
            _ if c == state => true,
            // Sealed beats unsealed; nothing replaces a `Sent`.
            (RecipientState::Sent { sealed: false }, RecipientState::Sent { sealed: true }) => {
                false
            }
            (RecipientState::Sent { .. }, _) => true,
            _ => false,
        };
        if keep {
            return Ok(());
        }
    }
    apply
        .conn
        .prepare_cached(
            "INSERT INTO send_state(message_id, user_id, state, error, at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(message_id, user_id) DO UPDATE SET
               state = excluded.state, error = excluded.error, at = excluded.at",
        )?
        .execute((message.0, col, sql::to_json(&state)?, error, apply.now))?;
    Ok(())
}

/// Sets the explicit floor of the message's outgoing state (`out_floor`) and recomputes the
/// aggregate. The outbox is the only writer of the floor of messages it sends.
fn set_floor(
    apply: &mut Apply<'_>,
    message: MessageId,
    conv: ConversationId,
    floor: &OutgoingState,
) -> Result<()> {
    apply
        .conn
        .prepare_cached("UPDATE messages SET out_floor = ?2 WHERE id = ?1")?
        .execute((message.0, sql::to_json(floor)?))?;
    apply.ch.message(conv, message);
    apply.recompute_out(message)
}

/// The stored features of `conv`; `None` while the network has stated none (NULL: sendable).
pub(crate) fn features_of(
    conn: &Connection,
    conv: ConversationId,
) -> Result<Option<ConversationFeatures>> {
    let features: Option<String> = conn
        .prepare_cached("SELECT features FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| r.get(0))?;
    sql::from_json_opt(features)
}

/// The current floor of a message.
pub(crate) fn floor(conn: &Connection, message: MessageId) -> Result<Option<OutgoingState>> {
    let f: Option<Option<String>> = conn
        .prepare_cached("SELECT out_floor FROM messages WHERE id = ?1")?
        .query_row([message.0], |r| r.get(0))
        .optional()?;
    sql::from_json_opt(f.flatten())
}

// ---------- attempts ----------

/// One attempt handed to the backend.
#[derive(Clone, Debug)]
pub(crate) struct Attempt {
    /// The outbox row.
    pub outbox: i64,
    /// Its conversation.
    pub conv: ConversationId,
    /// Its message row (`Command::Transmit.local`).
    pub message: MessageId,
    /// What to send.
    pub req: OutgoingRequest,
    /// The attachment rows of `req`'s `Body::Media.attachments`, by position: what the
    /// backend's `AttachmentProgress.index` names. Mapped once, here, with the join
    /// [`prune_gone_files`] uses (the request's blob path to the row that owns the file), after
    /// that function took files out of the request: the position is the request's, not the
    /// row's `idx`. `None` for an attachment the request holds no file of. Empty for anything
    /// but a message with files.
    pub attachments: Vec<Option<AttachmentId>>,
    /// Which attempt of the row this is (1 = the first), for the log.
    pub number: i64,
    /// When it started (ms), for the log's `took_ms`.
    pub started_at: i64,
}

/// What [`take_due`] found and started.
#[derive(Debug, Default)]
pub(crate) struct Due {
    /// Attempts now `Transmitting`, to send.
    pub attempts: Vec<Attempt>,
    /// The head row of every conversation with queued items.
    pub heads: BTreeMap<ConversationId, i64>,
    /// The earliest `next_attempt_at` of a head that is not due yet.
    pub next_wake: Option<i64>,
    /// Floors moved to `Sending`.
    pub changes: Changes,
}

/// A row the dispatcher must not start now: its conversation has a transmit in flight, or the
/// head is paused (until `ConfirmIdentity`).
#[derive(Debug, Default)]
pub(crate) struct Skip {
    /// Conversations with a transmit in flight.
    pub in_flight: BTreeSet<ConversationId>,
    /// Paused heads: conversation → outbox row.
    pub paused: BTreeMap<ConversationId, i64>,
}

/// The request of the next attempt: `only` narrowed to recipients that are not `Sent` yet
/// (§4.8 step 4); `None` stays "everyone" while nobody has it.
fn narrow(
    apply: &Apply<'_>,
    conv: ConversationId,
    message: MessageId,
    mut req: OutgoingRequest,
) -> Result<OutgoingRequest> {
    let states = send_states(apply.conn, message)?;
    let sent: BTreeSet<&str> = states
        .iter()
        .filter(|(_, s)| !s.needs_send())
        .map(|(u, _)| u.as_str())
        .collect();
    if sent.is_empty() {
        return Ok(req);
    }
    let pending: Vec<UserId> = match &req.only {
        Some(list) => list.clone(),
        None => {
            let mut all = recipients(apply, conv)?;
            for (u, s) in &states {
                let u = sql::user(u)?;
                if s.needs_send() && !all.contains(&u) {
                    all.push(u);
                }
            }
            all
        }
    };
    let mut only = Vec::new();
    for u in pending {
        let col = apply.author_col(Some(&u))?;
        if !col.is_empty() && !sent.contains(col.as_str()) && !only.contains(&u) {
            only.push(u);
        }
    }
    req.only = Some(only);
    req.is_recipient_update = true;
    Ok(req)
}

/// Whether the own send of `message` failed for everyone: its row is `Failed` (kept for
/// "Retry") and no recipient has the message. Its edits and reactions are then held back.
fn unsent_for_everyone(conn: &Connection, message: MessageId) -> Result<bool> {
    let failed: bool = conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM outbox WHERE message_id = ?1 AND kind = 'Message'
               AND state = ?2)",
        )?
        .query_row((message.0, FAILED), |r| r.get(0))?;
    Ok(failed
        && !send_states(conn, message)?
            .values()
            .any(|s| !s.needs_send()))
}

/// Sets the target of an own edit for the attempt that starts now (§3.5.3): the revision right
/// before its own, as the revisions are now. The request was written when the user typed the
/// edit and names the revision that was the latest then; if that one was an edit the network
/// refused, it is gone (rolled back) and the recipients never had it.
///
/// Only where the network names the latest revision ([`EditTarget::LatestRevision`], Signal).
/// Where it names the original ([`EditTarget::Original`]: WhatsApp, Threema) every edit of a
/// message names that one message, which `modify` wrote into the request when the user typed
/// it and which no rolled-back edit can change: there is nothing to move.
fn retarget_edit(
    conn: &Connection,
    edit_target: EditTarget,
    message: MessageId,
    req: &mut OutgoingRequest,
) -> Result<()> {
    if edit_target == EditTarget::Original {
        return Ok(());
    }
    let local = req.local.clone();
    let OutgoingKind::Edit { target, .. } = &mut req.kind else {
        return Ok(());
    };
    let revisions: Vec<(String, String)> = conn
        .prepare_cached(
            "SELECT net_author, net_local FROM message_revisions WHERE message_id = ?1
             ORDER BY revision",
        )?
        .query_map([message.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let own = revisions
        .iter()
        .position(|(author, l)| author.is_empty() && l.as_str() == local.as_ref());
    if let Some(previous) = own
        .and_then(|i| i.checked_sub(1))
        .and_then(|i| revisions.get(i))
        && target.local.as_ref() != previous.1
    {
        target.local = previous.1.as_str().into();
    }
    Ok(())
}

/// §4.8 step 2: the head of each conversation's FIFO that is due, not in flight and not paused
/// becomes `Transmitting` (at most `max` of them). A head whose remaining recipients all have
/// the message already is finished instead. Edits and reactions of a message whose own send
/// failed for everyone are passed over (held until the message is retried).
pub(crate) fn take_due(
    conn: &mut Connection,
    account: AccountId,
    rules: NetRules,
    now: i64,
    skip: &Skip,
    max: usize,
) -> Result<Due> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut apply = Apply::under_rules(&tx, account, rules, now)?;
    let heads = heads_of(&tx, account)?;
    let mut due = Due::default();
    for (outbox, conv, message, state, next, kind) in heads {
        let (conv, message) = (ConversationId(conv), MessageId(message));
        if state == QUEUED && kind != "Delete" && is_tombstone(&tx, message)? {
            // The message was deleted meanwhile (from another device, or by its author): it
            // is not sent any further, to the members a partial attempt missed, nor are its
            // edits and reactions. The conversation's next row is the head at the next look.
            tx.prepare_cached("DELETE FROM outbox WHERE id = ?1")?
                .execute([outbox])?;
            due.next_wake = Some(due.next_wake.map_or(now, |w| w.min(now)));
            continue;
        }
        due.heads.insert(conv, outbox);
        if state != QUEUED
            || skip.in_flight.contains(&conv)
            || skip.paused.get(&conv) == Some(&outbox)
        {
            continue;
        }
        // No wait is longer than the budget: one that is was set before the clock went back.
        let wait_until =
            next.filter(|at| *at > now && *at <= now.saturating_add(ms(rules.outbox.budget)));
        if let Some(at) = wait_until {
            due.next_wake = Some(due.next_wake.map_or(at, |w| w.min(at)));
            continue;
        }
        if due.attempts.len() >= max {
            continue;
        }
        let request: String = tx
            .prepare_cached("SELECT request FROM outbox WHERE id = ?1")?
            .query_row([outbox], |r| r.get(0))?;
        let mut req: OutgoingRequest = sql::from_json(&request)?;
        if kind == MESSAGE && prune_gone_files(&tx, message, &mut req)? {
            // A file of the message was deleted from another own device while its send
            // waited here (`AttachmentDeleted`): the message goes out as it is now, not
            // with a file whose blob is gone, which the backend could only fail on.
            if nothing_to_send(&req) {
                tx.prepare_cached("DELETE FROM outbox WHERE id = ?1")?
                    .execute([outbox])?;
                due.heads.remove(&conv);
                due.next_wake = Some(due.next_wake.map_or(now, |w| w.min(now)));
                continue;
            }
            tx.prepare_cached("UPDATE outbox SET request = ?2 WHERE id = ?1")?
                .execute((outbox, sql::to_json(&req)?))?;
        }
        if kind != MESSAGE {
            // An edit, delete or reaction: its own `only` (the members a partial attempt
            // missed); `send_state` belongs to the target's own send.
            retarget_edit(&tx, rules.edit_target, message, &mut req)?;
            // No stop mark outlives the attempt it was written for: a stale one is cleared
            // here too, so it can never be read as this attempt's (belt and braces to
            // `release` and `record_outcome`, which clear it with the attempt's end).
            let number: i64 = tx
                .prepare_cached(
                    "UPDATE outbox SET state = ?2, attempts = attempts + 1, next_attempt_at = NULL,
                       request = ?3, cancel_state = ?4 WHERE id = ?1 RETURNING attempts",
                )?
                .query_row(
                    (outbox, TRANSMITTING, sql::to_json(&req)?, CANCEL_NONE),
                    |r| r.get(0),
                )?;
            due.attempts.push(Attempt {
                outbox,
                conv,
                message,
                req,
                attachments: Vec::new(),
                number,
                started_at: now,
            });
            continue;
        }
        let req = narrow(&apply, conv, message, req)?;
        if req.only.as_ref().is_some_and(Vec::is_empty) {
            // Everyone it was meant for has it already (outcomes that arrived another way);
            // members that failed for good keep the row for a user retry.
            finish_attempt(&mut apply, outbox, conv, message, None, &req)?;
            continue;
        }
        // See above: a stale stop mark does not start with the attempt.
        let number: i64 = tx
            .prepare_cached(
                "UPDATE outbox SET state = ?2, attempts = attempts + 1, next_attempt_at = NULL,
                   request = ?3, cancel_state = ?4 WHERE id = ?1 RETURNING attempts",
            )?
            .query_row(
                (outbox, TRANSMITTING, sql::to_json(&req)?, CANCEL_NONE),
                |r| r.get(0),
            )?;
        let f = floor(&tx, message)?;
        if f.as_ref().is_none_or(|f| f.rank().is_none_or(|r| r < 1)) {
            set_floor(
                &mut apply,
                message,
                conv,
                &OutgoingState::Sending { progress: None },
            )?;
        }
        let attachments = attachment_ids(&tx, message, &req)?;
        due.attempts.push(Attempt {
            outbox,
            conv,
            message,
            req,
            attachments,
            number,
            started_at: now,
        });
    }
    apply.finish()?;
    due.changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    if !due.attempts.is_empty() {
        failpoint::hit(FAILPOINT_AFTER_ATTEMPT);
    }
    Ok(due)
}

/// One waiting row: (outbox, conversation, message, state, next_attempt_at, kind).
type Head = (i64, i64, i64, String, Option<i64>, String);

/// The head of every conversation of `account` that has waiting rows (`Queued` or
/// `Transmitting`), oldest row first: for each such conversation its oldest row that is not
/// held. Reads along the index of waiting rows (`outbox_waiting`): one step per conversation
/// that has any, and of each conversation only the rows up to its head (one, unless edits and
/// reactions of a message that failed for everyone are held in front). The cost of a look
/// follows the number of conversations with something to send, not the number of rows that
/// wait: a send of thousands of files is thousands of rows of one conversation, and the
/// dispatcher looks after every answer.
fn heads_of(conn: &Connection, account: AccountId) -> Result<Vec<Head>> {
    let mut heads: Vec<Head> = Vec::new();
    let mut held: BTreeMap<i64, bool> = BTreeMap::new();
    let mut after = i64::MIN;
    loop {
        let conv: Option<i64> = conn
            .prepare_cached(
                "SELECT min(conversation_id) FROM outbox
                 WHERE state IN ('Queued', 'Transmitting') AND conversation_id > ?1",
            )?
            .query_row([after], |r| r.get(0))?;
        let Some(conv) = conv else { break };
        after = conv;
        let ours: bool = conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ?1 AND account_id = ?2)",
            )?
            .query_row((conv, account.0), |r| r.get(0))?;
        if !ours {
            continue;
        }
        let mut waiting = conn.prepare_cached(
            "SELECT id, message_id, state, next_attempt_at, kind FROM outbox
             WHERE state IN ('Queued', 'Transmitting') AND conversation_id = ?1 ORDER BY id",
        )?;
        let mut rows = waiting.query([conv])?;
        while let Some(r) = rows.next()? {
            let row: Head = (r.get(0)?, conv, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            if matches!(row.5.as_str(), "Edit" | "React") {
                let hold = match held.get(&row.2) {
                    Some(h) => *h,
                    None => {
                        let h = unsent_for_everyone(conn, MessageId(row.2))?;
                        held.insert(row.2, h);
                        h
                    }
                };
                if hold {
                    continue;
                }
            }
            heads.push(row);
            break;
        }
    }
    // Oldest first, so the conversation that waited longest starts first when only some may.
    heads.sort_by_key(|h| h.0);
    Ok(heads)
}

/// The attachment rows of `message` that hold a file, by the file's blob path (relative to the
/// account's blob directory, as a request names it).
fn files_of(conn: &Connection, message: MessageId) -> Result<BTreeMap<String, AttachmentId>> {
    Ok(conn
        .prepare_cached(
            "SELECT f.path, a.id FROM attachments a JOIN files f ON f.id = a.file_id
             WHERE a.message_id = ?1",
        )?
        .query_map([message.0], |r| Ok((r.get(0)?, AttachmentId(r.get(1)?))))?
        .collect::<kourier_db::rusqlite::Result<_>>()?)
}

/// The attachment row of each attachment of `req`'s `Body::Media`, by position
/// ([`Attempt::attachments`]). Empty for a request that is not a message with files.
fn attachment_ids(
    conn: &Connection,
    message: MessageId,
    req: &OutgoingRequest,
) -> Result<Vec<Option<AttachmentId>>> {
    let OutgoingKind::Message {
        body: Body::Media { attachments, .. },
        ..
    } = &req.kind
    else {
        return Ok(Vec::new());
    };
    if attachments.iter().all(|a| a.local.is_none()) {
        return Ok(Vec::new());
    }
    let here = files_of(conn, message)?;
    Ok(attachments
        .iter()
        .map(|a| {
            a.local
                .as_ref()
                .and_then(|blob| here.get(blob.path.to_string_lossy().as_ref()).copied())
        })
        .collect())
}

/// Takes out of an own send's request the files that are no longer files of `message`: a
/// request names the blobs the backend reads, and an attachment deleted meanwhile (from
/// another own device, `AttachmentDeleted`) took its blob with it. Returns whether anything
/// went. A `Media` body left without files becomes its caption.
fn prune_gone_files(
    conn: &Connection,
    message: MessageId,
    req: &mut OutgoingRequest,
) -> Result<bool> {
    let OutgoingKind::Message { body, .. } = &mut req.kind else {
        return Ok(false);
    };
    let Body::Media { attachments, .. } = body else {
        return Ok(false);
    };
    if attachments.iter().all(|a| a.local.is_none()) {
        return Ok(false);
    }
    let here = files_of(conn, message)?;
    let before = attachments.len();
    attachments.retain(|a| {
        a.local
            .as_ref()
            .is_none_or(|blob| here.contains_key(blob.path.to_string_lossy().as_ref()))
    });
    if attachments.len() == before {
        return Ok(false);
    }
    if attachments.is_empty() {
        let caption = match std::mem::replace(body, Body::Text(Text::default())) {
            Body::Media { caption, .. } => caption,
            _ => None,
        };
        *body = Body::Text(caption.unwrap_or_default());
    }
    Ok(true)
}

/// Whether nothing is left of an own send's request to send (its files went and it had no
/// text).
fn nothing_to_send(req: &OutgoingRequest) -> bool {
    matches!(&req.kind, OutgoingKind::Message { body: Body::Text(t), .. } if t.body.trim().is_empty())
}

/// Whether `message` is a tombstone (deleted for everyone or for this account).
fn is_tombstone(conn: &Connection, message: MessageId) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT deleted IS NOT NULL FROM messages WHERE id = ?1")?
        .query_row([message.0], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}

/// Every recipient has the message: the floor is `Sent` and the row leaves the outbox.
fn finish_sent(
    apply: &mut Apply<'_>,
    outbox: i64,
    conv: ConversationId,
    message: MessageId,
    server_at: Option<i64>,
) -> Result<()> {
    apply
        .conn
        .prepare_cached("DELETE FROM outbox WHERE id = ?1")?
        .execute([outbox])?;
    mark_sent(apply, conv, message, server_at)
}

/// The server accepted the message (for some or all recipients): the earliest `server_at` and a
/// floor of at least `Sent`.
fn mark_sent(
    apply: &mut Apply<'_>,
    conv: ConversationId,
    message: MessageId,
    server_at: Option<i64>,
) -> Result<()> {
    apply
        .conn
        .prepare_cached(
            "UPDATE messages SET server_at = CASE WHEN server_at IS NULL THEN ?2
               WHEN ?2 IS NULL THEN server_at ELSE min(server_at, ?2) END WHERE id = ?1",
        )?
        .execute((message.0, server_at))?;
    let f = floor(apply.conn, message)?;
    if f.as_ref().is_none_or(|f| f.rank().is_none_or(|r| r < 2)) {
        set_floor(apply, message, conv, &OutgoingState::Sent { server_at })?;
    }
    Ok(())
}

/// Rows left `Transmitting` by an earlier actor or process go back to `Queued` (§4.8 step 4),
/// except a message whose stop the backend had accepted (`cancel_state` [`CANCEL_STOPPING`]):
/// that attempt reached nobody and the user asked for it to go, so it is TAKEN BACK here, with
/// its attachments' rows (the files go with the returned changes). A mark that was only asked
/// ([`CANCEL_ASKED`]) is cleared: the message may have gone, and it is sent as before.
/// Rows of tombstones other than deletes go: nothing in flight can answer them any more, and
/// a `Failed` one that an earlier build left behind would stay for good.
pub(crate) fn reset_transmitting(
    conn: &mut Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
) -> Result<(usize, Changes)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let stopped: Vec<(i64, i64)> = tx
        .prepare_cached(
            "SELECT message_id, conversation_id FROM outbox
             WHERE state = ?1 AND kind = ?2 AND cancel_state = ?3 AND conversation_id IN
               (SELECT id FROM conversations WHERE account_id = ?4)",
        )?
        .query_map((TRANSMITTING, MESSAGE, CANCEL_STOPPING, account.0), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut apply = apply_for(&tx, account, scope, now)?;
    for (message, conv) in &stopped {
        take_back(&tx, &mut apply, MessageId(*message), ConversationId(*conv))?;
    }
    let reset = tx
        .prepare_cached(
            "UPDATE outbox SET state = ?2, cancel_state = ?4 WHERE state = ?3 AND conversation_id IN
               (SELECT id FROM conversations WHERE account_id = ?1)",
        )?
        .execute((account.0, QUEUED, TRANSMITTING, CANCEL_NONE))?;
    // Per outbox row (few) one lookup of its message by id: the other way round, every message
    // of the account would be read at each actor start.
    tx.prepare_cached(
        "DELETE FROM outbox WHERE kind <> 'Delete' AND EXISTS
           (SELECT 1 FROM messages m WHERE m.id = outbox.message_id AND m.account_id = ?1
              AND m.deleted IS NOT NULL)",
    )?
    .execute([account.0])?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok((reset + stopped.len(), changes))
}

/// What the backend answered to a stop the user asked for ([`CANCEL_ASKED`]), written by the
/// dispatcher right after the answer and before it records anything else: `stopping` is the
/// durable `Stopping` mark, otherwise the mark goes (`TooLate`, no answer). `false` when no
/// row was asked any more (its outcome was recorded meanwhile, which cleared the mark).
pub(crate) fn answer_cancel(conn: &Connection, message: MessageId, stopping: bool) -> Result<bool> {
    let mark = if stopping {
        CANCEL_STOPPING
    } else {
        CANCEL_NONE
    };
    let changed = conn
        .prepare_cached(
            "UPDATE outbox SET cancel_state = ?2
             WHERE message_id = ?1 AND kind = ?3 AND state = ?4 AND cancel_state = ?5",
        )?
        .execute((message.0, mark, MESSAGE, TRANSMITTING, CANCEL_ASKED))?;
    if changed > 0 {
        failpoint::hit(FAILPOINT_AFTER_CANCEL_ANSWER);
    }
    Ok(changed > 0)
}

/// The clock went back (§9.6): every wait of `account`'s queued rows was set on the old clock
/// and would end late by the jump, so they are due now. Their attempt counts stay, so the
/// backoff goes on where it was.
pub(crate) fn clear_waits(conn: &Connection, account: AccountId) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "UPDATE outbox SET next_attempt_at = NULL WHERE state = ?2
               AND next_attempt_at IS NOT NULL AND conversation_id IN
               (SELECT id FROM conversations WHERE account_id = ?1)",
        )?
        .execute((account.0, QUEUED))?)
}

/// What [`release`] did.
#[derive(Debug, Default)]
pub(crate) struct Released {
    /// The row was in flight and is queued again (or its message was taken back).
    pub released: bool,
    /// What a take-back changed (the dispatcher publishes it; its files go with it).
    pub changes: Changes,
}

/// The outcome of an attempt could not be recorded (a `core.db` error): the row goes back to
/// `Queued` after a backoff instead of holding its conversation's FIFO as `Transmitting` until
/// the next actor start. The resend keeps the stored id and `only`; the backend's send progress
/// keeps it from reaching a recipient twice (§4.8 step 4). `released` is `false` when the row is
/// not in flight. The wait follows the account's back-off cap (`rules.outbox`).
///
/// A stop mark never outlives the attempt it was written for, so `release` follows
/// [`reset_transmitting`]'s rule in the same write: a message whose stop the backend had
/// ACCEPTED ([`CANCEL_STOPPING`]: the attempt reached nobody) is TAKEN BACK here, with its
/// attachments' rows (the files go with the returned changes), and a mark that was only asked
/// ([`CANCEL_ASKED`]) becomes [`CANCEL_NONE`]. Left on the row, the mark would be read by the
/// NEXT attempt, which was never asked to stop: a message the user stopped would go out, or a
/// message that may have reached its recipients would be taken back.
pub(crate) fn release(
    conn: &mut Connection,
    account: AccountId,
    rules: NetRules,
    outbox: i64,
    now: i64,
) -> Result<Released> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(i64, i64, i64, i64, String)> = tx
        .prepare_cached(
            "SELECT attempts, cancel_state, message_id, conversation_id, kind FROM outbox
             WHERE id = ?1 AND state = ?2",
        )?
        .query_row((outbox, TRANSMITTING), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    let Some((attempts, cancel_state, message, conv, kind)) = row else {
        return Ok(Released::default());
    };
    if kind == MESSAGE && cancel_state == CANCEL_STOPPING {
        let mut apply = Apply::under_rules(&tx, account, rules, now)?;
        take_back(&tx, &mut apply, MessageId(message), ConversationId(conv))?;
        apply.finish()?;
        let changes = std::mem::take(&mut apply.ch);
        drop(apply);
        tx.commit()?;
        return Ok(Released {
            released: true,
            changes,
        });
    }
    let wait = backoff_capped(
        sql::u32_of(attempts).saturating_add(1),
        rules.outbox.max_backoff,
    )
    .max(Duration::from_secs(1));
    tx.prepare_cached(
        "UPDATE outbox SET state = ?2, next_attempt_at = ?3, last_error = 'Internal',
           cancel_state = ?5
         WHERE id = ?1 AND state = ?4",
    )?
    .execute((
        outbox,
        QUEUED,
        now.saturating_add(ms(wait)),
        TRANSMITTING,
        CANCEL_NONE,
    ))?;
    tx.commit()?;
    Ok(Released {
        released: true,
        changes: Changes::default(),
    })
}

// ---------- outcomes ----------

/// A pause the dispatcher keeps in memory (a restart retries once and pauses again if needed).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Pause {
    /// `UntrustedIdentity`: this conversation waits for `ConfirmIdentity` of `user` or a cancel.
    Conversation {
        /// The conversation.
        conv: ConversationId,
        /// Its paused head.
        outbox: i64,
        /// Whose identity changed.
        user: UserId,
    },
    /// `Challenge` or a rejected login: the account's outbox waits.
    Account {
        /// The challenge the user must solve, if that is the reason.
        challenge: Option<Challenge>,
    },
}

/// A challenge of the network (`ErrorKind::Challenge`) the account's outbox waits on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Challenge {
    /// Where the user solves it.
    pub url: String,
    /// The network's token, handed back with the answer (`Command::SubmitChallenge`).
    pub token: Redacted<String>,
    /// When the network takes sends again without an answer (its Retry-After), if it said.
    pub retry_after: Option<Duration>,
}

/// A challenge an account waits on, as it is kept across a restart (`settings`, the
/// account's scope, key `challenge`). Signal Desktop keeps the conversations that wait for a
/// captcha the same way (`SD/ts/challenge.dom.ts`, storage key `challenge:conversations`) and
/// takes them up again at its next start. Without it every start would send one message into
/// a network that wants its challenge solved first, and the account would show the challenge
/// only after that send was refused.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredChallenge {
    /// Where the user solves it.
    pub url: String,
    /// The network's token of the challenged send (handed back with the answer). `None`: a
    /// job of the backend met the challenge (`BackendNotice::ChallengeRequired`); it is on
    /// show, and no send is held for it.
    #[serde(default)]
    pub token: Option<Redacted<String>>,
    /// When the network takes sends again without an answer (ms), if it said.
    #[serde(default)]
    pub retry_at: Option<i64>,
    /// When the wait began (ms). A send whose budget ends between this and the end of the
    /// wait has used its budget up waiting ([`give_up_overdue`]).
    pub since: i64,
}

impl StoredChallenge {
    /// Whether a challenge stored at `since` still counts at `now`: for as long as the
    /// network's policy says ([`OutboxPolicy::challenge_lifetime`]). Signal Desktop drops one
    /// after a day (`DEFAULT_EXPIRE_AFTER`), which is also the budget of every send it could
    /// hold. One dated in the future was stored on a clock that has been set back since (§9.6).
    pub(crate) fn is_fresh(&self, now: i64, policy: OutboxPolicy) -> bool {
        self.since <= now && now.saturating_sub(self.since) <= ms(policy.challenge_lifetime)
    }
}

const CHALLENGE_KEY: &str = "challenge";
const HOLD_KEY: &str = "hold";

/// The value of the account's `settings` row `key`, if it is there and this build can read it.
fn stored_setting<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    account: AccountId,
    key: &str,
) -> Result<Option<T>> {
    let json: Option<String> = conn
        .prepare_cached("SELECT value FROM settings WHERE scope = ?1 AND key = ?2")?
        .query_row((alloc_scope(account), key), |r| r.get(0))
        .optional()?;
    // A value this build cannot read is none: the next send finds out.
    Ok(json.and_then(|j| sql::from_json(&j).ok()))
}

/// Stores the account's `settings` row `key`, or forgets it (`None`).
fn store_setting<T: serde::Serialize>(
    conn: &Connection,
    account: AccountId,
    key: &str,
    value: Option<&T>,
) -> Result<()> {
    match value {
        Some(v) => {
            conn.prepare_cached(
                "INSERT INTO settings(scope, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
            )?
            .execute((alloc_scope(account), key, sql::to_json(v)?))?;
        }
        None => {
            conn.prepare_cached("DELETE FROM settings WHERE scope = ?1 AND key = ?2")?
                .execute((alloc_scope(account), key))?;
        }
    }
    Ok(())
}

/// The challenge stored for `account`, if any.
pub(crate) fn stored_challenge(
    conn: &Connection,
    account: AccountId,
) -> Result<Option<StoredChallenge>> {
    stored_setting(conn, account, CHALLENGE_KEY)
}

/// Stores the challenge `account` waits on, or forgets it (`None`).
pub(crate) fn store_challenge(
    conn: &Connection,
    account: AccountId,
    challenge: Option<&StoredChallenge>,
) -> Result<()> {
    store_setting(conn, account, CHALLENGE_KEY, challenge)
}

/// The send hold as `core.db` keeps it: what the backend stated, and when this core began to
/// hold (ms on the core's clock), so the time held can be given back to the rows' retry
/// budgets at the lift ([`give_back_held_time`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredHold {
    #[serde(flatten)]
    pub hold: AccountHold,
    /// When the hold began; none in a row an older build wrote (no time to give back).
    #[serde(default)]
    pub since_ms: Option<i64>,
}

/// The send hold stored for `account` (`{until_ms, reason, since_ms}`), if any. The row is the
/// hold as the backend last stated it: it is restored into the dispatcher at the first start of
/// a run, so a restart (a crash, a reboot, "Restart Now") does not send into a lock the backend
/// has not yet restated.
pub(crate) fn stored_hold(conn: &Connection, account: AccountId) -> Result<Option<StoredHold>> {
    stored_setting(conn, account, HOLD_KEY)
}

/// Stores the send hold of `account`, or forgets it (`None`: lifted, or ended by its time).
pub(crate) fn store_hold(
    conn: &Connection,
    account: AccountId,
    hold: Option<&StoredHold>,
) -> Result<()> {
    store_setting(conn, account, HOLD_KEY, hold)
}

/// The hold `held` ended at `now`: the time it stood does not count against a message's budget
/// (K-A fix C4), so every row of `account` that waited through it gets its deadline moved on
/// by that time. A row's budget began at its enqueue, which is its deadline minus the
/// account's budget (`policy`; every earlier hold moved the deadline by time that had passed
/// since, so that start is never after the hold's own for a row that is older than the hold):
/// a row made during the hold is given only the time since it was made. Returns the rows
/// changed.
///
/// Risk (K-B1, B6): a start is computed from the budget of the policy that is in force NOW. A
/// build that CHANGES a network's budget while rows wait in a hold computes their start from
/// the new number, so the time given back moves once, by the difference of the two budgets
/// (the deadlines themselves are stored and do not move). No network changes its budget in
/// K-B1; the step that gives WhatsApp its own numbers (W5) does it before any real send.
pub(crate) fn give_back_held_time(
    conn: &Connection,
    account: AccountId,
    held: &StoredHold,
    now: i64,
    policy: OutboxPolicy,
) -> Result<usize> {
    let Some(since) = held.since_ms else {
        return Ok(0);
    };
    // The hold ended by its own time at `until`, however late the core looked.
    let end = held
        .hold
        .until_ms
        .map_or(now, |u| now.min(i64::try_from(u).unwrap_or(i64::MAX)));
    Ok(conn
        .prepare_cached(
            "UPDATE outbox SET deadline_at = deadline_at + (?2 - max(?3, deadline_at - ?4))
               WHERE state IN (?5, ?6) AND ?2 > max(?3, deadline_at - ?4)
                 AND conversation_id IN (SELECT id FROM conversations WHERE account_id = ?1)",
        )?
        .execute((account.0, end, since, ms(policy.budget), QUEUED, STAGED))?)
}

impl Challenge {
    /// The challenge as it is stored when a send that ended at `now` was refused with it.
    pub(crate) fn stored(&self, now: i64) -> StoredChallenge {
        StoredChallenge {
            url: self.url.clone(),
            token: Some(self.token.clone()),
            retry_at: self.retry_after.map(|d| now.saturating_add(ms(d))),
            since: now,
        }
    }
}

/// The challenge among the per-member failures of a `Partial` outcome, if any.
pub(crate) fn challenge_of(failed: &[(UserId, BackendError)]) -> Option<Challenge> {
    failed.iter().find_map(|(_, e)| match &e.kind {
        ErrorKind::Challenge {
            token,
            url,
            retry_after,
        } => Some(Challenge {
            url: url.clone(),
            token: token.clone(),
            retry_after: *retry_after,
        }),
        _ => None,
    })
}

/// The user whose changed safety number kept a member of a `Partial` outcome from getting the
/// send (the first, when there are several: they are confirmed one after the other).
pub(crate) fn untrusted_of(failed: &[(UserId, BackendError)]) -> Option<UserId> {
    failed.iter().find_map(|(_, e)| match &e.kind {
        ErrorKind::UntrustedIdentity { user } => Some(user.clone()),
        _ => None,
    })
}

/// Whether a member that failed with `e` is sent to again once the pause its attempt ended in
/// is lifted (a challenge answered, a safety number confirmed): the members the network wants
/// the user for, and those that failed for a while. A member that failed for good is not.
pub(crate) fn waits_for_user(e: &BackendError) -> bool {
    e.is_retryable()
        || matches!(
            e.kind,
            ErrorKind::Challenge { .. } | ErrorKind::UntrustedIdentity { .. }
        )
}

/// An edit, delete or reaction the network refused for good (its local state was rolled back).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FailedChange {
    /// The UI action (`ActionFailed.action`).
    pub action: &'static str,
    /// The message the change was of.
    pub message: MessageId,
    /// Why.
    pub error: BackendError,
}

/// What [`record_outcome`] changed.
#[derive(Debug, Default)]
pub(crate) struct Recorded {
    pub changes: Changes,
    pub pause: Option<Pause>,
    /// The row waits this long for its next attempt (for the log).
    pub retry_in: Option<Duration>,
    /// An edit, delete or reaction the network refused for good: reported to the UI as
    /// `ActionFailed`, unless the user deleted its message meanwhile.
    pub failed: Option<FailedChange>,
}

/// How a failed attempt continues.
pub(crate) enum Next {
    /// Try again after this long (`only` may be narrowed).
    Retry(Duration),
    /// Wait for the user (`Pause`); the row stays the head.
    Pause(Pause),
    /// Terminal failure.
    Fail { reason: FailReason, retryable: bool },
}

/// Whether `n` is a wait for the user that a row past its budget does not enter (a
/// challenge or a safety number; a rejected login is not such a wait: what waits through a
/// logout goes out when the same account is linked again).
pub(crate) fn late_pause(n: &Next) -> bool {
    matches!(
        n,
        Next::Pause(Pause::Account { challenge: Some(_) } | Pause::Conversation { .. })
    )
}

/// How a failed attempt continues; a retryable error waits [`backoff_capped`] with the
/// account's cap (`policy`).
pub(crate) fn classify(
    e: &BackendError,
    conv: ConversationId,
    outbox: i64,
    failures: u32,
    policy: OutboxPolicy,
) -> Next {
    match &e.kind {
        ErrorKind::Transient { retry_after } | ErrorKind::RateLimited { retry_after } => {
            Next::Retry(
                backoff_capped(failures, policy.max_backoff).max(retry_after.unwrap_or_default()),
            )
        }
        ErrorKind::Challenge {
            url,
            token,
            retry_after,
        } => Next::Pause(Pause::Account {
            challenge: Some(Challenge {
                url: url.clone(),
                token: token.clone(),
                retry_after: *retry_after,
            }),
        }),
        ErrorKind::Unauthorized | ErrorKind::LoggedOut => {
            Next::Pause(Pause::Account { challenge: None })
        }
        ErrorKind::UntrustedIdentity { user } => Next::Pause(Pause::Conversation {
            conv,
            outbox,
            user: user.clone(),
        }),
        other => Next::Fail {
            reason: FailReason::from(other),
            retryable: false,
        },
    }
}

/// §4.8 step 3: writes one attempt's outcome (`None`: the backend dropped the reply, read as a
/// transient failure) in one transaction with the per-recipient `send_state`.
pub(crate) fn record_outcome(
    conn: &mut Connection,
    account: AccountId,
    rules: NetRules,
    now: i64,
    attempt: &Attempt,
    outcome: Option<TransmitOutcome>,
) -> Result<Recorded> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut apply = Apply::under_rules(&tx, account, rules, now)?;
    let outbox = attempt.outbox;
    // What was sent decides how the answer is read, not what the row says now: the answer to
    // an edit, delete or reaction whose row is gone says nothing about its target's own send.
    if !matches!(attempt.req.kind, OutgoingKind::Message { .. }) {
        let recorded = crate::modify::record_outcome(&mut apply, rules.outbox, attempt, outcome)?;
        if let Some(Pause::Account {
            challenge: Some(c), ..
        }) = &recorded.pause
        {
            store_challenge(&tx, account, Some(&c.stored(now)))?;
        }
        apply.finish()?;
        let changes = std::mem::take(&mut apply.ch);
        drop(apply);
        failpoint::hit(FAILPOINT_BEFORE_OUTCOME_COMMIT);
        tx.commit()?;
        failpoint::hit(FAILPOINT_AFTER_OUTCOME);
        return Ok(Recorded {
            changes,
            ..recorded
        });
    }
    // Outbox ids are never reused, so a row with this id is the row of this attempt. A merge
    // of two conversations may have moved it and its message meanwhile (§3.3): the answer is
    // recorded where they are now. Without the row (an echo from another device finished it)
    // the per-recipient outcome still counts for the message.
    let row: Option<(i64, u32, i64, i64)> = tx
        .prepare_cached(
            "SELECT message_id, attempts, deadline_at, cancel_state FROM outbox
             WHERE id = ?1 AND state = ?2",
        )?
        .query_row((outbox, TRANSMITTING), |r| {
            Ok((r.get(0)?, sql::u32_of(r.get(1)?), r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let message = row.map_or(attempt.message, |(m, _, _, _)| MessageId(m));
    let cancel_state = row.map_or(CANCEL_NONE, |(_, _, _, c)| c);
    let row = row.map(|(_, attempts, deadline, _)| (attempts, deadline));
    let conv: Option<i64> = tx
        .prepare_cached("SELECT conversation_id FROM messages WHERE id = ?1")?
        .query_row([message.0], |r| r.get(0))
        .optional()?;
    let Some(conv) = conv.map(ConversationId) else {
        // The message went while the attempt was in flight (expired, its conversation
        // removed): nothing is left to record.
        return Ok(Recorded::default());
    };
    // The backend said `Stopping` ([`CANCEL_STOPPING`]): nothing of this attempt went out, so
    // whatever it ends with other than a send that went out anyway (a backend bug, below) the
    // message the user stopped is taken back. A mark that was only asked is no promise: the
    // outcome is read as it comes and the mark goes with the row's next state.
    if cancel_state == CANCEL_STOPPING {
        let went = matches!(
            &outcome,
            Some(
                TransmitOutcome::Sent { .. }
                    | TransmitOutcome::SentAs { .. }
                    | TransmitOutcome::Partial { .. }
            )
        );
        if went {
            tracing::error!(
                account = account.0,
                msg = message.0,
                "an attempt that was stopped reported a send: recorded as sent"
            );
        } else {
            take_back(&tx, &mut apply, message, conv)?;
            apply.finish()?;
            let changes = std::mem::take(&mut apply.ch);
            drop(apply);
            failpoint::hit(FAILPOINT_BEFORE_OUTCOME_COMMIT);
            tx.commit()?;
            failpoint::hit(FAILPOINT_AFTER_OUTCOME);
            return Ok(Recorded {
                changes,
                ..Recorded::default()
            });
        }
    }
    let outcome = outcome.unwrap_or_else(|| {
        TransmitOutcome::Failed(BackendError::new(
            ErrorKind::Transient { retry_after: None },
            "the backend dropped the transmit reply",
        ))
    });
    let mut pause = None;
    let mut retry_in = None;
    // How a failed attempt continues: the decision, the request of the next attempt and the
    // log-safe error name stored in `last_error`.
    let mut next: Option<(Next, OutgoingRequest, &'static str)> = None;
    // Somebody has it now: a disappearing timer starts (once).
    let reached = match &outcome {
        TransmitOutcome::Sent { .. } | TransmitOutcome::SentAs { .. } => true,
        TransmitOutcome::Partial { delivered, .. } => !delivered.is_empty(),
        TransmitOutcome::Failed(_) => false,
    };
    if reached {
        apply.start_send_timer(message)?;
    }
    // The server gave the message an id of its own (KT-2, [`crate::ingest::Apply::rekey_own`]):
    // its keys and what the queue names move before the send is finished. Also when the row is
    // gone (an echo finished it) and for a tombstone: a delete queued behind it must name that id.
    let rekeyed = match &outcome {
        TransmitOutcome::SentAs { assigned, .. } => {
            apply.rekey_own(message, &assigned.local, Some(assigned.seq))?
        }
        _ => None,
    };
    match outcome {
        TransmitOutcome::Sent {
            server_at,
            per_recipient,
        }
        | TransmitOutcome::SentAs {
            server_at,
            per_recipient,
            ..
        } => {
            for (u, s) in &per_recipient {
                set_recipient(&mut apply, message, u, *s, None)?;
            }
            // Everyone this attempt went to has it, listed or not: `only`, or for a send to
            // everyone every recipient still waiting in `send_state`.
            let targets = match &attempt.req.only {
                Some(only) => only.clone(),
                None => waiting(&tx, message)?,
            };
            for u in &targets {
                if !per_recipient.iter().any(|(p, _)| p == u) {
                    let sent = RecipientState::Sent { sealed: false };
                    set_recipient(&mut apply, message, u, sent, None)?;
                }
            }
            if row.is_some() {
                finish_attempt(&mut apply, outbox, conv, message, server_at, &attempt.req)?;
            }
        }
        TransmitOutcome::Partial { delivered, failed } => {
            for (u, s) in &delivered {
                set_recipient(&mut apply, message, u, *s, None)?;
            }
            // A member the network wants a challenge solved for, or whose safety number
            // changed, is not lost: with the row still here, they wait with it for the user.
            // A challenge comes first: nothing is sent until it is answered, and the next
            // attempt meets the safety number again.
            let pause = match (challenge_of(&failed), untrusted_of(&failed)) {
                _ if row.is_none() => None,
                (Some(c), _) => Some((Pause::Account { challenge: Some(c) }, "Challenge")),
                (None, Some(user)) => Some((
                    Pause::Conversation { conv, outbox, user },
                    "UntrustedIdentity",
                )),
                (None, None) => None,
            };
            let paused = pause.is_some();
            for (u, e) in &failed {
                let state = RecipientState::Failed {
                    retryable: e.is_retryable() || (paused && waits_for_user(e)),
                };
                set_recipient(&mut apply, message, u, state, Some(e.kind.name()))?;
            }
            if let Some((attempts, deadline)) = row {
                let retry: Vec<&(UserId, BackendError)> =
                    failed.iter().filter(|(_, e)| e.is_retryable()).collect();
                if let Some((pause, name)) = pause {
                    // §4.8 `Challenge` and `UntrustedIdentity`, for some members only: the
                    // pause is that of a whole send (the account's outbox for a challenge,
                    // this conversation's queue for a safety number) and the row stays the
                    // head, narrowed to the members that do not have the message (those the
                    // user is needed for and those that failed for a while). Members that
                    // failed for good are not asked again. Signal Desktop stops the
                    // conversation's queue the same way
                    // (`SD/ts/jobs/conversationJobQueue.preload.ts`, verification pending)
                    // while the members it reached keep the message.
                    let members: Vec<UserId> = failed
                        .iter()
                        .filter(|(_, e)| waits_for_user(e))
                        .map(|(u, _)| u.clone())
                        .collect();
                    let mut req = attempt.req.resend_to(members);
                    if delivered.is_empty() {
                        // Nobody has a first copy yet: not a follow-up send.
                        req.is_recipient_update = attempt.req.is_recipient_update;
                    }
                    next = Some((Next::Pause(pause), req, name));
                } else if let Some((_, first)) = retry.first() {
                    // A rate-limited member holds the conversation's queue for the wait.
                    let wait = retry
                        .iter()
                        .map(|(_, e)| e.kind.retry_after().unwrap_or_default())
                        .max()
                        .unwrap_or_default()
                        .max(backoff_capped(attempts, rules.outbox.max_backoff));
                    let n = if now.saturating_add(ms(wait)) > deadline {
                        Next::Fail {
                            reason: FailReason::from(&first.kind),
                            retryable: true,
                        }
                    } else {
                        Next::Retry(wait)
                    };
                    let members = retry.iter().map(|(u, _)| u.clone()).collect();
                    next = Some((n, attempt.req.resend_to(members), first.kind.name()));
                } else if delivered.is_empty() {
                    let (reason, name) = failed.first().map_or(
                        (FailReason::Other("no recipient".into()), "Internal"),
                        |(_, e)| (FailReason::from(&e.kind), e.kind.name()),
                    );
                    let n = Next::Fail {
                        reason,
                        retryable: false,
                    };
                    next = Some((n, attempt.req.clone(), name));
                } else {
                    // Some have it, the rest failed for good: `PartiallyFailed` from the
                    // aggregate; the row stays `Failed` for a user retry of those members.
                    finish_attempt(&mut apply, outbox, conv, message, None, &attempt.req)?;
                }
            }
        }
        TransmitOutcome::Failed(e) => {
            if let Some((attempts, deadline)) = row {
                let n = match classify(&e, conv, outbox, attempts, rules.outbox) {
                    Next::Retry(wait) if now.saturating_add(ms(wait)) > deadline => Next::Fail {
                        reason: FailReason::from(&e.kind),
                        retryable: true,
                    },
                    n => n,
                };
                next = Some((n, attempt.req.clone(), e.kind.name()));
            }
        }
    }
    // A row past its budget was still sent once (it waited where waiting does not count:
    // offline, logged out, or behind another pause). That attempt was its last one. When the
    // answer asks for the user (a challenge, a safety number), the row does not go on waiting
    // for an answer that can take days: it fails like one whose last attempt failed, and the
    // user's "Retry" starts it again. The account still waits on the challenge.
    let overdue = row.is_some_and(|(_, deadline)| now > deadline);
    if overdue && let Some((Next::Pause(p), req, error)) = next.take_if(|(n, _, _)| late_pause(n)) {
        let reason = match &p {
            Pause::Account { .. } => FailReason::Challenge,
            Pause::Conversation { .. } => FailReason::UntrustedIdentity,
        };
        if matches!(p, Pause::Account { .. }) {
            pause = Some(p);
        }
        let n = Next::Fail {
            reason,
            retryable: true,
        };
        next = Some((n, req, error));
    }
    if let Some((n, req, error)) = next {
        let req = sql::to_json(&req)?;
        match n {
            Next::Retry(wait) => {
                retry_in = Some(wait);
                tx.prepare_cached(
                    "UPDATE outbox SET state = ?2, next_attempt_at = ?3, request = ?4,
                       last_error = ?5 WHERE id = ?1",
                )?
                .execute((
                    outbox,
                    QUEUED,
                    now.saturating_add(ms(wait)),
                    req,
                    error,
                ))?;
            }
            Next::Pause(p) => {
                tx.prepare_cached(
                    "UPDATE outbox SET state = ?2, next_attempt_at = NULL, request = ?3,
                       last_error = ?4 WHERE id = ?1",
                )?
                .execute((outbox, QUEUED, req, error))?;
                if matches!(p, Pause::Conversation { .. }) {
                    let shown = OutgoingState::Failed {
                        reason: FailReason::UntrustedIdentity,
                        retryable: true,
                    };
                    set_floor(&mut apply, message, conv, &shown)?;
                }
                pause = Some(p);
            }
            Next::Fail { reason, retryable } => {
                tx.prepare_cached(
                    "UPDATE outbox SET state = ?2, next_attempt_at = NULL, request = ?3,
                       last_error = ?4 WHERE id = ?1",
                )?
                .execute((outbox, FAILED, req, error))?;
                for (u, s) in send_states(&tx, message)? {
                    if matches!(s, RecipientState::Pending) {
                        let u = sql::user(&u)?;
                        let failed = RecipientState::Failed { retryable };
                        set_recipient(&mut apply, message, &u, failed, None)?;
                    }
                }
                if reason == FailReason::ReachOutRefused {
                    fail_waiting_of(&tx, &mut apply, conv, message, &reason)?;
                }
                set_floor(
                    &mut apply,
                    message,
                    conv,
                    &OutgoingState::Failed { reason, retryable },
                )?;
            }
        }
    }
    // A stop that was asked but did not stop this attempt (the backend said `TooLate`, said
    // nothing, or the attempt ended on its own first) is over with the attempt: the row goes on
    // without the mark, so the next attempt is never taken for a stopped one.
    if cancel_state != CANCEL_NONE {
        tx.prepare_cached("UPDATE outbox SET cancel_state = ?2 WHERE id = ?1")?
            .execute((outbox, CANCEL_NONE))?;
    }
    // The message was deleted (here, from another device, by its author) while this attempt
    // was in flight: nothing more of it is sent, and no row of it is kept for a retry.
    if is_tombstone(&tx, message)? {
        tx.prepare_cached("DELETE FROM outbox WHERE id = ?1 AND kind <> 'Delete'")?
            .execute([outbox])?;
    }
    // The challenge the account waits on from now, in the transaction that recorded the
    // answer: a kill right behind it finds the pause at the next start.
    if let Some(Pause::Account {
        challenge: Some(c), ..
    }) = &pause
    {
        store_challenge(&tx, account, Some(&c.stored(now)))?;
    }
    // The answer names who the message went to (`send_state`), also someone the
    // conversation's member list does not have: a receipt of theirs that overtook this
    // answer waited (§4.9) and counts now.
    apply.retry_receipts_of(message)?;
    apply.ch.message(conv, message);
    apply.recompute_out(message)?;
    // Last: what waited under the server's id may remove the message.
    if let Some(done) = rekeyed {
        apply.settle_rekey(done)?;
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    failpoint::hit(FAILPOINT_BEFORE_OUTCOME_COMMIT);
    tx.commit()?;
    failpoint::hit(FAILPOINT_AFTER_OUTCOME);
    Ok(Recorded {
        changes,
        pause,
        retry_in,
        failed: None,
    })
}

/// A network refused `message` as a first contact ([`FailReason::ReachOutRefused`]): the
/// conversation's other messages that still wait for their first attempt fail the same way,
/// without one. Each of them is one more reach-out to the same person, and every reach-out
/// counts against the account (research §5.5: after a 463 stop, explain, never retry). A
/// message in flight already has its own answer coming, and the rows of a large send that is
/// still being stored (`Staged`) are messages that wait for their first attempt too (K-A fix
/// C6: they would turn `Queued` a moment later and be one more reach-out); edits, reactions and deletes are not
/// messages to a new person and keep waiting for their target. There is no account-wide hold
/// (K-A decision 5): a message typed afterwards is the user's own new act.
fn fail_waiting_of(
    tx: &Connection,
    apply: &mut Apply<'_>,
    conv: ConversationId,
    except: MessageId,
    reason: &FailReason,
) -> Result<()> {
    let waiting: Vec<(i64, i64)> = tx
        .prepare_cached(
            "SELECT id, message_id FROM outbox
             WHERE conversation_id = ?1 AND kind = ?2 AND state IN (?3, ?5) AND message_id <> ?4
             ORDER BY id",
        )?
        .query_map((conv.0, MESSAGE, QUEUED, except.0, STAGED), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    for (outbox, message) in waiting {
        fail_unattempted(tx, apply, conv, outbox, MessageId(message), reason)?;
    }
    Ok(())
}

/// One row that waited for its first attempt fails as a first contact the network refused
/// ([`fail_waiting_of`]): no attempt, not retryable, shown as the floor.
fn fail_unattempted(
    tx: &Connection,
    apply: &mut Apply<'_>,
    conv: ConversationId,
    outbox: i64,
    message: MessageId,
    reason: &FailReason,
) -> Result<()> {
    tx.prepare_cached(
        "UPDATE outbox SET state = ?2, next_attempt_at = NULL, last_error = ?3
         WHERE id = ?1",
    )?
    .execute((outbox, FAILED, ErrorKind::ReachOutRefused.name()))?;
    for (u, s) in send_states(tx, message)? {
        if matches!(s, RecipientState::Pending) {
            let failed = RecipientState::Failed { retryable: false };
            set_recipient(apply, message, &sql::user(&u)?, failed, None)?;
        }
    }
    let shown = OutgoingState::Failed {
        reason: reason.clone(),
        retryable: false,
    };
    set_floor(apply, message, conv, &shown)
}

/// Whether one of `messages` (the messages of a send) has the floor "refused as a first
/// contact": the network answered a 463 while the send was stored in parts.
fn reach_out_refused_in(tx: &Connection, messages: &[MessageId]) -> Result<bool> {
    for message in messages {
        if matches!(
            floor(tx, *message)?,
            Some(OutgoingState::Failed {
                reason: FailReason::ReachOutRefused,
                ..
            })
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The `Staged` rows of `messages` (a send whose last part was just stored) fail as a first
/// contact the network refused, like the rows [`fail_waiting_of`] met.
fn fail_staged_of(
    tx: &Connection,
    apply: &mut Apply<'_>,
    conv: ConversationId,
    messages: &[MessageId],
) -> Result<()> {
    for message in messages {
        let row: Option<i64> = tx
            .prepare_cached(
                "SELECT id FROM outbox WHERE message_id = ?1 AND kind = ?2 AND state = ?3",
            )?
            .query_row((message.0, MESSAGE, STAGED), |r| r.get(0))
            .optional()?;
        if let Some(outbox) = row {
            fail_unattempted(
                tx,
                apply,
                conv,
                outbox,
                *message,
                &FailReason::ReachOutRefused,
            )?;
        }
    }
    Ok(())
}

/// What [`give_up_overdue`] did.
#[derive(Debug, Default)]
pub(crate) struct GaveUp {
    /// Rows to invalidate.
    pub changes: Changes,
    /// Edits, deletes and reactions that were rolled back, to report.
    pub failed: Vec<FailedChange>,
    /// The earliest budget end of a row that still waits.
    pub next_deadline: Option<i64>,
}

/// A wait for the user that holds rows of an account ([`give_up_overdue`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Wait {
    /// The rows it holds: those of this conversation (its queue waits for a safety number
    /// to be confirmed), or with `None` every row of the account (a challenge).
    pub conv: Option<ConversationId>,
    /// When the wait began (ms).
    pub since: i64,
    /// When the wait ends by itself (ms), if it does: the time the network gave with a
    /// challenge. The wait holds rows no longer than that, whenever the core gets to look:
    /// a budget that ended later ended while the row waited for something else (the
    /// machine slept through the end of the challenge, Kourier was not running), where
    /// waiting does not count.
    pub until: Option<i64>,
}

/// §4.8 `Challenge` and `UntrustedIdentity`: the wait for the user counts against the retry
/// budget of every row it holds. A waiting row whose budget ended during one of `waits`
/// (after it began; by `now`, or by the time the wait ends by itself when that came first,
/// [`Wait::until`]) gives up as it would after a last attempt: a message becomes
/// `Failed` and retryable ("Retry" gives it a fresh budget and its first id), an edit, delete
/// or reaction is rolled back and reported. Without this a message typed before a challenge,
/// or before a contact's safety number changed, would go out whenever the user gets to it,
/// days later, and an edit or delete would then arrive after the receive windows of the other
/// clients, which are the send windows plus this budget. Signal Desktop's send jobs wait for
/// a captcha or a verification no longer than their day either
/// (`SD/ts/jobs/conversationJobQueue.preload.ts` `timeRemaining`).
///
/// A row whose budget had ended before the wait began is left alone: it waited where
/// waiting does not count (offline, logged out), and it still gets the one late attempt
/// such a row gets when the account is back; the wait only stands in front of it. The same
/// holds for a row whose budget ended after a challenge had run out by its own time while
/// nothing looked (a suspend, a restart): it is tried when the pause is lifted. Held
/// edits and reactions (of a message that failed for everyone) are left alone too: their
/// budget starts when the message is retried.
pub(crate) fn give_up_overdue(
    conn: &mut Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
    waits: &[Wait],
) -> Result<GaveUp> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut apply = apply_for(&tx, account, scope, now)?;
    let mut gave = GaveUp::default();
    for wait in waits {
        // (outbox, conversation, message, kind, deadline), oldest first: a message gives up
        // before its edits and reactions are looked at, which are held from then on. Only
        // the rows whose budget ended during the wait are read.
        type Row = (i64, i64, i64, String, i64);
        let conv = wait.conv.map(|c| c.0);
        // The wait held rows up to its own end, not up to this look.
        let end = wait.until.map_or(now, |until| until.min(now));
        let over: Vec<Row> = tx
            .prepare_cached(
                "SELECT o.id, o.conversation_id, o.message_id, o.kind, o.deadline_at
                 FROM outbox o JOIN conversations c ON c.id = o.conversation_id
                 WHERE c.account_id = ?1 AND o.state = ?2 AND o.deadline_at > ?3
                   AND o.deadline_at <= ?4 AND (?5 IS NULL OR o.conversation_id = ?5)
                 ORDER BY o.id",
            )?
            .query_map((account.0, QUEUED, wait.since, end, conv), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let (reason, name) = match wait.conv {
            None => (FailReason::Challenge, "Challenge"),
            Some(_) => (FailReason::UntrustedIdentity, "UntrustedIdentity"),
        };
        for (outbox, conv, message, kind, _) in over {
            let (conv, message) = (ConversationId(conv), MessageId(message));
            if matches!(kind.as_str(), "Edit" | "React") && unsent_for_everyone(&tx, message)? {
                continue;
            }
            if kind != MESSAGE {
                let error = BackendError::new(
                    ErrorKind::Transient { retry_after: None },
                    "the retry budget ran out while the send waited for the user",
                );
                gave.failed
                    .extend(crate::modify::give_up(&mut apply, outbox, error)?);
                continue;
            }
            tx.prepare_cached(
                "UPDATE outbox SET state = ?2, next_attempt_at = NULL, last_error = ?3
                 WHERE id = ?1",
            )?
            .execute((outbox, FAILED, name))?;
            for (u, s) in send_states(&tx, message)? {
                if matches!(s, RecipientState::Pending) {
                    let failed = RecipientState::Failed { retryable: true };
                    set_recipient(&mut apply, message, &sql::user(&u)?, failed, None)?;
                }
            }
            let shown = OutgoingState::Failed {
                reason: reason.clone(),
                retryable: true,
            };
            set_floor(&mut apply, message, conv, &shown)?;
        }
        // The next budget that ends during this wait.
        let next: Option<i64> = tx
            .prepare_cached(
                "SELECT min(o.deadline_at) FROM outbox o
                   JOIN conversations c ON c.id = o.conversation_id
                 WHERE c.account_id = ?1 AND o.state = ?2 AND o.deadline_at > ?3
                   AND (?4 IS NULL OR o.conversation_id = ?4)
                   AND (?5 IS NULL OR o.deadline_at <= ?5)",
            )?
            .query_row(
                (account.0, QUEUED, now.max(wait.since), conv, wait.until),
                |r| r.get(0),
            )?;
        if let Some(next) = next {
            gave.next_deadline = Some(gave.next_deadline.map_or(next, |d| d.min(next)));
        }
    }
    apply.finish()?;
    gave.changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(gave)
}

/// Recipients of `message` whose `send_state` says they do not have it yet.
fn waiting(conn: &Connection, message: MessageId) -> Result<Vec<UserId>> {
    send_states(conn, message)?
        .into_iter()
        .filter(|(_, s)| s.needs_send())
        .map(|(u, _)| sql::user(&u))
        .collect()
}

/// An attempt reached everyone it went to. When no recipient is left waiting the row is done
/// ([`finish_sent`]); when some failed for good in an earlier or this attempt, the floor is
/// `Sent` (the aggregate shows `PartiallyFailed`) and the row is kept `Failed` with
/// `only` = those members, so "Retry" can reach them with the same id (§4.8 step 5).
fn finish_attempt(
    apply: &mut Apply<'_>,
    outbox: i64,
    conv: ConversationId,
    message: MessageId,
    server_at: Option<i64>,
    req: &OutgoingRequest,
) -> Result<()> {
    let left = waiting(apply.conn, message)?;
    if left.is_empty() {
        return finish_sent(apply, outbox, conv, message, server_at);
    }
    apply
        .conn
        .prepare_cached(
            "UPDATE outbox SET state = ?2, next_attempt_at = NULL, request = ?3,
               last_error = 'attempt failed' WHERE id = ?1",
        )?
        .execute((outbox, FAILED, sql::to_json(&req.resend_to(left))?))?;
    mark_sent(apply, conv, message, server_at)
}

// ---------- user actions ----------

/// The outbox row of a message: id, state.
pub(crate) fn row_of(
    conn: &Connection,
    message: MessageId,
) -> Result<Option<(i64, String, String)>> {
    Ok(conn
        .prepare_cached(
            "SELECT id, state, request FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
        )?
        .query_row([message.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?)
}

/// The account and conversation of the own message `message`, for "Retry" and "Cancel". One
/// that is hidden because its time has come on an unconfirmed clock ([`crate::expiry`]) is
/// gone for the user: a command that still names it is refused like one about a message
/// that is not there.
fn own_message(conn: &Connection, message: MessageId) -> Result<(AccountId, ConversationId)> {
    let row: Option<(i64, i64, bool)> = conn
        .prepare_cached(
            "SELECT account_id, conversation_id, expiry_hidden FROM messages
             WHERE id = ?1 AND net_author = ''",
        )?
        .query_row([message.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let (account, conv, hidden) =
        row.ok_or(CoreError::refused(Refusal::NotOwn, "not an own message"))?;
    if hidden {
        return Err(CoreError::refused(Refusal::Gone, "unknown message"));
    }
    Ok((AccountId(account), ConversationId(conv)))
}

/// §4.8 step 5 "Retry": the failed (or paused) message is queued again with its original id,
/// a fresh budget, and `only` = the recipients that do not have it yet.
pub(crate) fn retry(
    conn: &mut Connection,
    rules: NetRules,
    message: MessageId,
    now: i64,
) -> Result<Written> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (account, conv) = own_message(&tx, message)?;
    let mut apply = Apply::under_rules(&tx, account, rules, now)?;
    let (outbox, state, request) =
        row_of(&tx, message)?.ok_or(CoreError::InvalidInput("the message has nothing to retry"))?;
    if state == TRANSMITTING {
        return Err(CoreError::refused(
            Refusal::BeingSent,
            "the message is being sent",
        ));
    }
    // The same rule the flag `MessageActions::retry` answers: a message the network refused as
    // a reach-out is not sent again, and neither is one into a chat that lost the right to
    // send since it failed (a left group, an un-accepted chat).
    crate::actions::retry(
        floor(&tx, message)?.as_ref(),
        features_of(&tx, conv)?.as_ref(),
    )?;
    let mut req: OutgoingRequest = sql::from_json(&request)?;
    // A retry does not forget what an earlier attempt may have left with the server: a row
    // that waited for its next attempt, or gave up on attempts without a definite answer,
    // keeps one attempt on record. Until the retried send is answered the message is then
    // still not "ours to take back" (`actions::sending_of`): a delete for everyone of it is
    // sent and an edit is a revision. A row the network refused for good starts from nothing.
    let maybe_out = matches!(
        crate::actions::sending_of(&tx, message, false)?,
        crate::actions::Sending::Unsent { attempted: true }
    );
    let states = send_states(&tx, message)?;
    let any_sent = states.values().any(|s| !s.needs_send());
    let mut only = Vec::new();
    for (u, s) in &states {
        if s.needs_send() {
            let u = sql::user(u)?;
            set_recipient(&mut apply, message, &u, RecipientState::Pending, None)?;
            only.push(u);
        }
    }
    if any_sent {
        req.only = Some(only);
        req.is_recipient_update = true;
    } else {
        req.only = None;
        req.is_recipient_update = false;
    }
    tx.prepare_cached(
        "UPDATE outbox SET state = ?2, attempts = ?5, next_attempt_at = NULL, deadline_at = ?3,
           last_error = NULL, request = ?4 WHERE id = ?1",
    )?
    .execute((
        outbox,
        QUEUED,
        now.saturating_add(ms(rules.outbox.budget)),
        sql::to_json(&req)?,
        i64::from(maybe_out),
    ))?;
    // Its edits and reactions that were held while nobody had it follow it now: their budget
    // starts with the message's.
    tx.prepare_cached(
        "UPDATE outbox SET deadline_at = ?2, next_attempt_at = NULL
         WHERE message_id = ?1 AND kind IN ('Edit', 'React') AND state = ?3",
    )?
    .execute((
        message.0,
        now.saturating_add(ms(rules.outbox.budget)),
        QUEUED,
    ))?;
    set_floor(&mut apply, message, conv, &OutgoingState::Queued)?;
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(Written {
        account,
        conv,
        changes,
    })
}

/// Removes `message` of `conv` with its outbox row, its attachments' rows and (after the
/// commit, by the changes) their files; the conversation's counts follow. The one body of a
/// take-back: the user's cancel of a message that waits ([`cancel`]) and a stopped attempt
/// ([`record_outcome`], [`reset_transmitting`]).
fn take_back(
    tx: &Connection,
    apply: &mut Apply<'_>,
    message: MessageId,
    conv: ConversationId,
) -> Result<()> {
    // Its imported files go with it (unlinked after the commit).
    apply.delete_attachments(message)?;
    tx.prepare_cached("DELETE FROM messages WHERE id = ?1")?
        .execute([message.0])?;
    apply.recount(conv)?;
    apply.ch.conversations.insert(conv);
    apply.ch.timelines.insert(conv);
    Ok(())
}

/// What [`cancel`] did.
#[derive(Debug)]
pub(crate) enum Cancelled {
    /// The message was taken back (it was waiting).
    TakenBack(Written),
    /// The message is in flight and uploading: the stop is ASKED ([`CANCEL_ASKED`] is written);
    /// the dispatcher now asks the backend, and its answer decides ([`answer_cancel`]).
    Asked(Written),
}

/// §4.8 step 5: a queued or failed message that no recipient has and that no attempt may have
/// reached the server for ([`actions::sending_of`], the rule of the flag `take_back`) is deleted
/// with its outbox row. A message in flight is taken back here only as far as the user's wish goes: when it is
/// [`actions::stoppable`] (the backend reports an upload of its first attempt and nobody has
/// it) the stop is written down ([`CANCEL_ASKED`], durable) and the message stays until the
/// backend has answered; any other message in flight is refused. `uploading`: the core's set of
/// reported uploads holds the message; the caller reads it INSIDE the write, so the rule is
/// judged on the state the row has here.
pub(crate) fn cancel(
    conn: &mut Connection,
    scope: MsgIdScope,
    message: MessageId,
    now: i64,
    uploading: bool,
) -> Result<Cancelled> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (account, conv) = own_message(&tx, message)?;
    let (_, state, _) =
        row_of(&tx, message)?.ok_or(CoreError::InvalidInput("the message is not queued"))?;
    if state == TRANSMITTING {
        let (attempts, cancel_state): (i64, i64) = tx
            .prepare_cached(
                "SELECT attempts, cancel_state FROM outbox WHERE message_id = ?1 AND kind = ?2",
            )?
            .query_row((message.0, MESSAGE), |r| Ok((r.get(0)?, r.get(1)?)))?;
        let stoppable = actions::stoppable(actions::StopFacts {
            transmitting: true,
            uploading,
            attempts,
            reached: uploading
                && attempts == 1
                && send_states(&tx, message)?.values().any(|s| !s.needs_send()),
            asked: cancel_state != CANCEL_NONE,
        });
        if !stoppable {
            return Err(CoreError::refused(
                Refusal::BeingSent,
                "the message is being sent",
            ));
        }
        tx.prepare_cached(
            "UPDATE outbox SET cancel_state = ?2 WHERE message_id = ?1 AND kind = ?3",
        )?
        .execute((message.0, CANCEL_ASKED, MESSAGE))?;
        // The row's flags are read again: the X greys out.
        let mut changes = Changes::default();
        changes.message(conv, message);
        tx.commit()?;
        failpoint::hit(FAILPOINT_AFTER_CANCEL_ASKED);
        return Ok(Cancelled::Asked(Written {
            account,
            conv,
            changes,
        }));
    }
    // The flag's own rule ([`actions::cancel_sending`]: a take-back is offered for a message
    // nothing was handed to the network for), judged here on the state the row has INSIDE this
    // write: a retry wait after an attempt that may have reached the server (a dropped reply, a
    // temporary failure) is refused like a message in flight, whatever the X on screen said.
    match actions::sending_of(&tx, message, false)? {
        Sending::Unsent { attempted: false } => {}
        Sending::Unsent { attempted: true } => {
            return Err(CoreError::refused(
                Refusal::BeingSent,
                "an attempt to send the message may have reached the server",
            ));
        }
        _ => {
            return Err(CoreError::InvalidInput(
                "the message reached some recipients already",
            ));
        }
    }
    let mut apply = apply_for(&tx, account, scope, now)?;
    take_back(&tx, &mut apply, message, conv)?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(Cancelled::TakenBack(Written {
        account,
        conv,
        changes,
    }))
}

/// Of the paused conversations `convs`, the GROUPS of `account` that `user` is a member of
/// (critic M4/F19): a confirm of one member resumes a group paused on another, whose head
/// then retries (it pauses again on a member the backend still refuses: a Signal member in
/// state 2; a member who was never verified is trusted at the retry, the lead's R1). One lookup per paused
/// conversation (a handful), each by primary keys ([`PAUSED_GROUP_MEMBER`], plan test).
pub(crate) fn groups_with_member(
    conn: &Connection,
    account: AccountId,
    convs: &[ConversationId],
    user: &UserId,
) -> Result<Vec<ConversationId>> {
    let group = sql::enum_name(&kourier_model::ConvKind::Group)?;
    let mut stmt = conn.prepare_cached(PAUSED_GROUP_MEMBER)?;
    let mut out = Vec::new();
    for conv in convs {
        let is: bool = stmt.query_row((conv.0, user.as_str(), account.0, &group), |r| r.get(0))?;
        if is {
            out.push(*conv);
        }
    }
    Ok(out)
}

/// Whether conversation ?1 is a group of account ?3 (kind ?4) with member ?2: the
/// conversation by its rowid, the member by the primary key `(conversation_id, user_id)`,
/// never `members_by_user` (a user is in many groups).
pub(crate) const PAUSED_GROUP_MEMBER: &str = "SELECT EXISTS(SELECT 1 FROM conversations c
   WHERE c.id = ?1 AND c.account_id = ?3 AND c.kind = ?4
     AND EXISTS(SELECT 1 FROM members m WHERE m.conversation_id = c.id AND m.user_id = ?2))";

/// After `ConfirmIdentity`: the paused heads show `Queued` again until their next attempt.
pub(crate) fn requeue(
    conn: &mut Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
    outbox_rows: &[i64],
) -> Result<Changes> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut apply = apply_for(&tx, account, scope, now)?;
    for id in outbox_rows {
        let row: Option<(i64, i64)> = tx
            .prepare_cached(
                "SELECT message_id, conversation_id FROM outbox
                 WHERE id = ?1 AND state = ?2 AND kind = 'Message'",
            )?
            .query_row((id, QUEUED), |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((message, conv)) = row {
            set_floor(
                &mut apply,
                MessageId(message),
                ConversationId(conv),
                &OutgoingState::Queued,
            )?;
        }
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(changes)
}

/// Stores (or clears) the composer draft of a conversation (core-local).
pub(crate) fn save_draft(
    conn: &Connection,
    conv: ConversationId,
    draft: Option<&Text>,
) -> Result<()> {
    let json = draft
        .filter(|t| !t.body.is_empty())
        .map(sql::to_json)
        .transpose()?;
    let n = conn
        .prepare_cached("UPDATE conversations SET draft = ?2 WHERE id = ?1")?
        .execute((conv.0, json))?;
    if n == 0 {
        return Err(CoreError::UnknownConversation(conv));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lookup of a confirm's groups goes by primary keys: no scan, and not the index of
    /// every membership of the user (core.db has no planner statistics, `core.md`).
    #[test]
    fn the_paused_group_lookup_goes_by_primary_keys() {
        let dir = tempfile::tempdir().unwrap();
        let key = kourier_db::DbKey::from([7u8; 32]);
        let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).unwrap();
        kourier_db::migrate(&mut conn, &crate::store::MIGRATIONS).unwrap();
        let steps: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {PAUSED_GROUP_MEMBER}"))
            .unwrap()
            .query_map((1_i64, "aci:x", 1_i64, "Group"), |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<kourier_db::rusqlite::Result<_>>()
            .unwrap();
        assert!(
            steps
                .iter()
                .all(|s| (s == "SCAN CONSTANT ROW" || !s.starts_with("SCAN"))
                    && !s.contains("members_by_user")),
            "{steps:#?}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.contains("members") && s.contains("conversation_id=? AND user_id=?")),
            "{steps:#?}"
        );
        assert!(
            steps
                .iter()
                .any(|s| s.starts_with("SEARCH c USING INTEGER PRIMARY KEY")),
            "{steps:#?}"
        );
    }

    /// An unrecordable outcome hands the row back to the queue after a backoff; a row that is
    /// not in flight is left alone.
    #[test]
    fn release_queues_a_row_whose_outcome_was_lost() {
        let dir = tempfile::tempdir().unwrap();
        let key = kourier_db::DbKey::from([7u8; 32]);
        let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).unwrap();
        kourier_db::migrate(&mut conn, &crate::store::MIGRATIONS).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
               deadline_at) VALUES (1, 1, 1, 'Message', '{}', 'Transmitting', 3, 0),
                                   (2, 2, 1, 'Message', '{}', 'Queued', 1, 0)",
            [],
        )
        .unwrap();
        let signal = NetRules::signal(MsgIdScope::AccountAuthor);
        let released = |conn: &mut Connection, rules: NetRules, row: i64| {
            release(conn, AccountId(1), rules, row, 1_000)
                .unwrap()
                .released
        };
        assert!(released(&mut conn, signal, 1));
        let (state, next): (String, i64) = conn
            .query_row(
                "SELECT state, next_attempt_at FROM outbox WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, QUEUED);
        // backoff(4) = 685 ms, raised to the 1 s floor.
        assert_eq!(next, 1_000 + 1_000);
        assert!(!released(&mut conn, signal, 2), "not in flight");
        assert!(!released(&mut conn, signal, 1), "already released");
        assert!(!released(&mut conn, signal, 9), "gone");
    }

    /// The dispatcher's look at the outbox reads one head per conversation along the index
    /// of waiting rows, whatever number of rows waits: a send of thousands of files is
    /// thousands of rows of one conversation, and the look runs after every answer.
    #[test]
    fn a_look_at_the_outbox_reads_heads_not_rows() {
        let dir = tempfile::tempdir().unwrap();
        let key = kourier_db::DbKey::from([7u8; 32]);
        let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).unwrap();
        kourier_db::migrate(&mut conn, &crate::store::MIGRATIONS).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO conversations(id, account_id, conv_key, kind) VALUES
               (1, 1, 'd:a', 'Direct'), (2, 1, 'd:b', 'Direct'), (3, 2, 'd:c', 'Direct'),
               (4, 1, 'd:d', 'Direct');",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO outbox(message_id, conversation_id, kind, request, state,
                       deadline_at) VALUES (?1, ?2, ?3, '{}', ?4, 0)",
                )
                .unwrap();
            // Conversation 2 first (the oldest row), then 5,000 rows of conversation 1, a
            // row of another account, and rows that do not wait.
            insert.execute((1, 2, "Message", "Queued")).unwrap();
            for m in 0..5_000 {
                let state = if m == 0 { "Transmitting" } else { "Queued" };
                insert.execute((100 + m, 1, "Message", state)).unwrap();
            }
            insert.execute((9_000, 3, "Message", "Queued")).unwrap();
            insert.execute((9_001, 4, "Message", "Failed")).unwrap();
            insert.execute((9_002, 4, "Message", "Staged")).unwrap();
            // Held: a reaction to a message whose own send failed for everyone; behind it
            // a delete, which is the head.
            insert.execute((9_001, 4, "React", "Queued")).unwrap();
            insert.execute((9_003, 4, "Delete", "Queued")).unwrap();
        }
        tx.commit().unwrap();

        let heads = heads_of(&conn, AccountId(1)).unwrap();
        let seen: Vec<(i64, i64, &str, &str)> = heads
            .iter()
            .map(|h| (h.1, h.2, h.3.as_str(), h.5.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (2, 1, "Queued", "Message"),
                (1, 100, "Transmitting", "Message"),
                (4, 9_003, "Queued", "Delete"),
            ],
            "one head per conversation of the account, oldest row first"
        );
        assert_eq!(heads_of(&conn, AccountId(2)).unwrap().len(), 1);
        assert!(heads_of(&conn, AccountId(3)).unwrap().is_empty());

        // Both statements walk the index of waiting rows; neither scans the table.
        for sql in [
            "SELECT min(conversation_id) FROM outbox
             WHERE state IN ('Queued', 'Transmitting') AND conversation_id > ?1",
            "SELECT id, message_id, state, next_attempt_at, kind FROM outbox
             WHERE state IN ('Queued', 'Transmitting') AND conversation_id = ?1 ORDER BY id",
        ] {
            let plan: Vec<String> = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([1], |r| r.get(3))
                .unwrap()
                .collect::<kourier_db::rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter().any(|p| p.contains("outbox_waiting")),
                "{plan:?}"
            );
            assert!(
                !plan
                    .iter()
                    .any(|p| p.contains("SCAN") && !p.contains("INDEX")),
                "{plan:?}"
            );
            assert!(!plan.iter().any(|p| p.contains("TEMP B-TREE")), "{plan:?}");
        }
        // The look does not grow with the rows that wait behind the heads.
        let started = std::time::Instant::now();
        for _ in 0..200 {
            assert_eq!(heads_of(&conn, AccountId(1)).unwrap().len(), 3);
        }
        let each = started.elapsed() / 200;
        // For comparison, what the look read before: every waiting row of the account.
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let rows: Vec<(i64, i64, i64, String, Option<i64>, String)> = conn
                .prepare_cached(
                    "SELECT o.id, o.conversation_id, o.message_id, o.state, o.next_attempt_at,
                       o.kind FROM outbox o JOIN conversations c ON c.id = o.conversation_id
                     WHERE c.account_id = ?1 AND o.state IN ('Queued', 'Transmitting')
                     ORDER BY o.id",
                )
                .unwrap()
                .query_map([1], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })
                .unwrap()
                .collect::<kourier_db::rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(rows.len(), 5_003);
        }
        let before = started.elapsed() / 20;
        eprintln!("a look with 5,000 waiting rows: {each:?} (every row: {before:?})");
        assert!(each < Duration::from_millis(5), "{each:?}");
    }

    /// A request names the blobs the backend reads. A file that is no longer a file of the
    /// message is taken out of the request; with all of them gone the caption is what is
    /// left, and without one nothing is.
    #[test]
    fn a_request_loses_the_files_its_message_lost() {
        let dir = tempfile::tempdir().unwrap();
        let key = kourier_db::DbKey::from([7u8; 32]);
        let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).unwrap();
        kourier_db::migrate(&mut conn, &crate::store::MIGRATIONS).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO files(id, account_id, path, key, purpose)
               VALUES (1, 1, 'core/aa/kept', x'00', 'attachment');
             INSERT INTO attachments(message_id, idx, kind, state, file_id)
               VALUES (7, 0, 'Image', '\"Local\"', 1);",
        )
        .unwrap();
        let file = |path: &str| kourier_model::Attachment {
            local: Some(BlobRef {
                path: path.into(),
                key: BlobKey::new([0; 32]),
            }),
            ..Default::default()
        };
        let request = |attachments, caption: Option<&str>| OutgoingRequest {
            conv: ConvKey::Direct(UserId::parse(ALICE).unwrap()),
            local: "300".into(),
            sent_at: 300,
            kind: OutgoingKind::Message {
                body: Body::Media {
                    attachments,
                    caption: caption.map(Text::plain),
                },
                reply_to: None,
                expiry: None,
            },
            only: None,
            is_recipient_update: false,
        };
        let files = |req: &OutgoingRequest| match &req.kind {
            OutgoingKind::Message {
                body: Body::Media { attachments, .. },
                ..
            } => attachments.len(),
            _ => 0,
        };
        let mut req = request(vec![file("core/aa/gone"), file("core/aa/kept")], None);
        assert!(prune_gone_files(&conn, MessageId(7), &mut req).unwrap());
        assert_eq!(files(&req), 1);
        assert!(!nothing_to_send(&req));
        assert!(
            !prune_gone_files(&conn, MessageId(7), &mut req).unwrap(),
            "nothing more to take out"
        );
        // All files gone: the caption alone, or nothing.
        let mut req = request(vec![file("core/aa/gone")], Some("look"));
        assert!(prune_gone_files(&conn, MessageId(7), &mut req).unwrap());
        assert!(matches!(
            &req.kind,
            OutgoingKind::Message { body: Body::Text(t), .. } if t.body == "look"
        ));
        assert!(!nothing_to_send(&req));
        let mut req = request(vec![file("core/aa/gone")], None);
        assert!(prune_gone_files(&conn, MessageId(7), &mut req).unwrap());
        assert!(nothing_to_send(&req));
        // A text, and files the request does not point at (nothing to read), stay.
        let mut req = request(vec![kourier_model::Attachment::default()], None);
        assert!(!prune_gone_files(&conn, MessageId(7), &mut req).unwrap());
    }

    /// The backend's `AttachmentProgress.index` is a position in the attempt's request, which
    /// lost a file meanwhile: `take_due` maps each position to the attachment row that owns the
    /// file (a position, not the row's `idx`).
    #[test]
    fn an_attempt_names_the_attachment_rows_of_its_requests_files() {
        use crate::attachments::import::{Imported, Written};
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::{remote, signal_features};
        use kourier_model::{AttachmentKind, ConversationPatch};

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let mut patch = ConversationPatch::new(key.clone());
        patch.features = Some(signal_features(false));
        h.apply(vec![
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(remote(&key, &alice, "100", 100, "hello")),
        ])
        .unwrap();
        let account = h.account();
        let conn = h.connection_mut();
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("ids", vec![UserId::parse(ME).unwrap()]),
        );
        let picture = |i: usize| Imported {
            kind: AttachmentKind::Image,
            content: AttachmentKind::Image,
            file_name: None,
            dims: Some((40, 30)),
            blurhash: None,
            file: Written {
                path: format!("core/aa/{i:032x}").into(),
                key: kourier_blob::FileKey::from_bytes([1; 32]),
                size: 10,
                mime: "image/png".into(),
            },
            thumb: None,
        };
        let files: std::sync::Arc<[Imported]> = (0..3).map(picture).collect();
        match enqueue_files(conn, &net, conv, files, None, None, 1_000) {
            Ok(FilesStep::Done(..)) => {}
            other => panic!("not stored whole: {other:?}"),
        }
        let rules = NetRules::signal(MsgIdScope::AccountAuthor);
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM attachments ORDER BY idx")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<kourier_db::rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids.len(), 3, "one album of three pictures");

        // Whole: the positions are the rows in order.
        let due = take_due(conn, account, rules, 2_000, &Skip::default(), 4).unwrap();
        let [attempt] = &due.attempts[..] else {
            panic!("{due:?}")
        };
        assert_eq!(
            attempt.attachments,
            ids.iter()
                .map(|id| Some(AttachmentId(*id)))
                .collect::<Vec<_>>()
        );

        // The second picture is deleted from another own device before the next attempt
        // (the attempt failed and waits again): the request loses it, and position 1 is now
        // the THIRD picture's row, not the row with `idx` 1.
        conn.execute(
            "UPDATE outbox SET state = 'Queued', next_attempt_at = NULL",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM attachments WHERE id = ?1", [ids[1]])
            .unwrap();
        let due = take_due(conn, account, rules, 3_000, &Skip::default(), 4).unwrap();
        let [attempt] = &due.attempts[..] else {
            panic!("{due:?}")
        };
        assert_eq!(
            attempt.attachments,
            [Some(AttachmentId(ids[0])), Some(AttachmentId(ids[2]))],
            "the positions of the request that goes out, not the rows' idx"
        );
    }

    /// A send of more messages than one transaction writes is stored in parts whose rows
    /// the dispatcher does not see; the last part makes all of them due at once and clears
    /// the draft. A send that fails between two parts is dropped whole, with the rows of
    /// its files, and the files are handed to the unlink.
    #[test]
    fn a_large_send_is_staged_in_parts_and_dropped_whole_when_one_fails() {
        use crate::attachments::import::{Imported, Written};
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::{remote, signal_features};
        use kourier_model::{AttachmentKind, ConversationPatch};

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let mut patch = ConversationPatch::new(key.clone());
        patch.features = Some(signal_features(false));
        h.apply(vec![
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(remote(&key, &alice, "100", 100, "hello")),
        ])
        .unwrap();
        let account = h.account();
        let conn = h.connection_mut();
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        save_draft(conn, conv, Some(&Text::plain("the caption"))).unwrap();
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("staged", vec![UserId::parse(ME).unwrap()]),
        );
        // 150 documents: each travels alone, so 150 messages in three parts.
        let n = 2 * ENQUEUE_CHUNK + 22;
        let files: std::sync::Arc<[Imported]> = (0..n)
            .map(|i| Imported {
                kind: AttachmentKind::File,
                content: AttachmentKind::File,
                file_name: Some(format!("doc-{i:03}.txt")),
                dims: None,
                blurhash: None,
                file: Written {
                    path: format!("core/aa/{i:032x}").into(),
                    key: kourier_blob::FileKey::from_bytes([1; 32]),
                    size: 10,
                    mime: "text/plain".into(),
                },
                thumb: None,
            })
            .collect();
        let count = |conn: &Connection, sql: &str| id_of(conn, sql);
        let part = |step: std::result::Result<FilesStep, FilesError>| match step {
            Ok(FilesStep::More(send)) => send,
            other => panic!("not a part: {other:?}"),
        };
        let caption = Some(Text::plain("the caption"));

        // The first part: stored, and invisible to the dispatcher.
        let first = part(enqueue_files(
            conn,
            &net,
            conv,
            files.clone(),
            caption.clone(),
            None,
            1_000,
        ));
        let chunk = i64::try_from(ENQUEUE_CHUNK).unwrap();
        assert_eq!(
            count(conn, "SELECT count(*) FROM outbox WHERE state = 'Staged'"),
            chunk
        );
        assert_eq!(count(conn, "SELECT count(*) FROM files"), chunk);
        let due = take_due(
            conn,
            account,
            NetRules::signal(MsgIdScope::AccountAuthor),
            2_000,
            &Skip::default(),
            4,
        )
        .unwrap();
        assert!(due.attempts.is_empty() && due.heads.is_empty(), "{due:?}");
        assert_eq!(
            count(
                conn,
                "SELECT count(*) FROM conversations WHERE draft IS NOT NULL"
            ),
            1,
            "the draft stays until the whole send is stored"
        );
        // It fails here (the app is closing, the chat was blocked): nothing of it stays.
        let stored = first.stored().to_vec();
        assert_eq!(stored.len(), ENQUEUE_CHUNK);
        drop(first);
        let dropped = discard_staged(conn, MsgIdScope::AccountAuthor, &stored, 3_000).unwrap();
        assert_eq!(
            dropped.unlink.len(),
            ENQUEUE_CHUNK,
            "its blobs are unlinked"
        );
        assert!(dropped.timelines.contains(&conv));
        assert_eq!(count(conn, "SELECT count(*) FROM outbox"), 0);
        assert_eq!(count(conn, "SELECT count(*) FROM files"), 0);
        assert_eq!(count(conn, "SELECT count(*) FROM attachments"), 0);
        assert_eq!(count(conn, "SELECT count(*) FROM messages"), 1, "alice's");
        assert!(
            discard_staged(conn, MsgIdScope::AccountAuthor, &stored, 3_000)
                .unwrap()
                .unlink
                .is_empty()
        );

        // Again, to the end: three parts, then every row is due and the draft is gone.
        let mut step = enqueue_files(conn, &net, conv, files.clone(), caption, None, 4_000);
        let mut parts = 1;
        let (written, messages) = loop {
            match step {
                Ok(FilesStep::Done(written, messages)) => break (written, messages),
                Ok(FilesStep::More(send)) => {
                    assert_eq!(
                        count(conn, "SELECT count(*) FROM outbox WHERE state = 'Queued'"),
                        0,
                        "nothing is due before the last part"
                    );
                    parts += 1;
                    step = enqueue_files_more(conn, &net, send, 4_000).map_err(FilesError::Other);
                }
                Err(e) => panic!("{e:?}"),
            }
        };
        assert_eq!(parts, 3);
        assert_eq!(messages.len(), n);
        assert!(messages.windows(2).all(|w| w[0].0 < w[1].0), "in order");
        assert_eq!(
            written.changes.messages.get(&conv).map(|m| m.len()),
            Some(n)
        );
        let all = i64::try_from(n).unwrap();
        assert_eq!(
            count(conn, "SELECT count(*) FROM outbox WHERE state = 'Queued'"),
            all
        );
        assert_eq!(count(conn, "SELECT count(*) FROM outbox"), all);
        assert_eq!(
            count(
                conn,
                "SELECT count(*) FROM conversations WHERE draft IS NOT NULL"
            ),
            0
        );
        // The caption rides on the first message only.
        assert_eq!(
            count(
                conn,
                "SELECT count(*) FROM messages WHERE body LIKE '%the caption%'"
            ),
            1
        );
        let due = take_due(
            conn,
            account,
            NetRules::signal(MsgIdScope::AccountAuthor),
            5_000,
            &Skip::default(),
            4,
        )
        .unwrap();
        assert_eq!(due.attempts.len(), 1, "one conversation: one head");
        assert_eq!(due.attempts[0].message, messages[0]);

        // What a kill leaves of a send is dropped at the next start.
        conn.execute("UPDATE outbox SET state = 'Staged' WHERE id % 2 = 0", [])
            .unwrap();
        let half = count(conn, "SELECT count(*) FROM outbox WHERE state = 'Staged'");
        assert_eq!(
            discard_all_staged(conn).unwrap(),
            usize::try_from(half).unwrap()
        );
        assert_eq!(count(conn, "SELECT count(*) FROM outbox"), all - half);
        assert_eq!(count(conn, "SELECT count(*) FROM files"), all - half);
        assert_eq!(count(conn, "SELECT count(*) FROM attachments"), all - half);
        assert_eq!(discard_all_staged(conn).unwrap(), 0);
    }

    /// A large send into a chat that is hidden from the list (deleted, empty). Storing a
    /// message lists a chat again, but a part of a send that is only `Staged` is not there
    /// yet: when the send is dropped, by a failure of a later part or by a kill, the chat
    /// is as hidden as before, not an empty chat in the list. The transaction that queues
    /// the send lists it. A message that arrives between two parts lists it for good.
    #[test]
    fn a_dropped_large_send_leaves_a_hidden_chat_hidden() {
        use crate::attachments::import::{Imported, Written};
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::{remote, signal_features};
        use kourier_model::{AttachmentKind, ConversationPatch};

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let mut patch = ConversationPatch::new(key.clone());
        patch.features = Some(signal_features(false));
        h.apply(vec![BackendEvent::ConversationUpsert(patch)])
            .unwrap();
        let conv = ConversationId(id_of(h.connection(), "SELECT id FROM conversations"));
        // Deleted while empty: out of the list until something is stored or queued there.
        h.clear_conversation(conv, true).unwrap();
        let listed = |h: &crate::testing::IngestHarness| {
            h.conversations(&Default::default())
                .unwrap()
                .rows
                .iter()
                .any(|c| c.id == conv)
        };
        assert!(!listed(&h));
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("staged", vec![UserId::parse(ME).unwrap()]),
        );
        let n = ENQUEUE_CHUNK + 6;
        let files: std::sync::Arc<[Imported]> = (0..n)
            .map(|i| Imported {
                kind: AttachmentKind::File,
                content: AttachmentKind::File,
                file_name: Some(format!("doc-{i:03}.txt")),
                dims: None,
                blurhash: None,
                file: Written {
                    path: format!("core/aa/{i:032x}").into(),
                    key: kourier_blob::FileKey::from_bytes([1; 32]),
                    size: 10,
                    mime: "text/plain".into(),
                },
                thumb: None,
            })
            .collect();
        let part = |step: std::result::Result<FilesStep, FilesError>| match step {
            Ok(FilesStep::More(send)) => send,
            other => panic!("not a part: {other:?}"),
        };
        let messages = |h: &crate::testing::IngestHarness| {
            id_of(h.connection(), "SELECT count(*) FROM messages")
        };

        // A later part fails: the first is dropped, the chat is hidden as before.
        let first = part(enqueue_files(
            h.connection_mut(),
            &net,
            conv,
            files.clone(),
            None,
            None,
            1_000,
        ));
        assert_eq!(messages(&h), i64::try_from(ENQUEUE_CHUNK).unwrap());
        assert!(!listed(&h), "a staged part lists nothing");
        let stored = first.stored().to_vec();
        drop(first);
        discard_staged(
            h.connection_mut(),
            MsgIdScope::AccountAuthor,
            &stored,
            2_000,
        )
        .unwrap();
        assert_eq!(messages(&h), 0);
        assert!(!listed(&h), "dropped: hidden as before");

        // A kill between two parts: the next start drops what was stored.
        drop(part(enqueue_files(
            h.connection_mut(),
            &net,
            conv,
            files.clone(),
            None,
            None,
            3_000,
        )));
        assert_eq!(
            discard_all_staged(h.connection_mut()).unwrap(),
            ENQUEUE_CHUNK
        );
        assert_eq!(messages(&h), 0);
        assert!(!listed(&h), "dropped at the start: hidden as before");

        // To its end: the transaction that queues the send lists the chat.
        let first = part(enqueue_files(
            h.connection_mut(),
            &net,
            conv,
            files.clone(),
            None,
            None,
            4_000,
        ));
        assert!(!listed(&h));
        match enqueue_files_more(h.connection_mut(), &net, first, 4_000) {
            Ok(FilesStep::Done(written, sent)) => {
                assert_eq!(sent.len(), n);
                assert!(written.changes.conversations.contains(&conv));
            }
            other => panic!("not the end: {other:?}"),
        }
        assert!(listed(&h), "queued: the chat is back");

        // Hidden again; this time a message arrives between two parts, and stays when
        // the send is dropped: the chat is listed for it.
        h.connection().execute("DELETE FROM messages", []).unwrap();
        h.clear_conversation(conv, true).unwrap();
        assert!(!listed(&h));
        let first = part(enqueue_files(
            h.connection_mut(),
            &net,
            conv,
            files,
            None,
            None,
            5_000,
        ));
        h.apply(vec![BackendEvent::Message(remote(
            &key, &alice, "900", 900, "hello",
        ))])
        .unwrap();
        assert!(listed(&h));
        let stored = first.stored().to_vec();
        drop(first);
        discard_staged(
            h.connection_mut(),
            MsgIdScope::AccountAuthor,
            &stored,
            6_000,
        )
        .unwrap();
        assert_eq!(messages(&h), 1, "alice's");
        assert!(listed(&h), "not hidden again over a message that is there");
    }

    /// The first part of a send of 150 documents to alice's chat under her PNI, stored
    /// (`Staged`), and then the merge of that chat into her chat under her ACI (§3.3): the
    /// stored messages and their outbox rows are in the chat that stayed, the chat the send
    /// was started in is gone.
    fn a_part_whose_chat_was_merged(
        dir: &tempfile::TempDir,
    ) -> (
        crate::testing::IngestHarness,
        kourier_fake::FakeNetwork,
        Box<FilesSend>,
        ConversationId,
    ) {
        use crate::attachments::import::{Imported, Written};
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::{remote, signal_features};
        use kourier_model::{AttachmentKind, ConversationPatch};

        let (alice, pni) = (
            UserId::parse(ALICE).unwrap(),
            UserId::parse(ALICE_PNI).unwrap(),
        );
        let (key, pni_key) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(pni.clone()));
        let mut h = harness(dir, MsgIdScope::AccountAuthor);
        let mut patch = ConversationPatch::new(pni_key.clone());
        patch.features = Some(signal_features(false));
        h.apply(vec![
            BackendEvent::Message(remote(&key, &alice, "100", 100, "hello")),
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(remote(&pni_key, &pni, "200", 200, "hello again")),
        ])
        .unwrap();
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("merged", vec![UserId::parse(ME).unwrap()]),
        );
        let files: std::sync::Arc<[Imported]> = (0..2 * ENQUEUE_CHUNK + 22)
            .map(|i| Imported {
                kind: AttachmentKind::File,
                content: AttachmentKind::File,
                file_name: Some(format!("doc-{i:03}.txt")),
                dims: None,
                blurhash: None,
                file: Written {
                    path: format!("core/aa/{i:032x}").into(),
                    key: kourier_blob::FileKey::from_bytes([1; 32]),
                    size: 10,
                    mime: "text/plain".into(),
                },
                thumb: None,
            })
            .collect();
        let old_conv = ConversationId(
            h.connection()
                .query_row(
                    "SELECT id FROM conversations WHERE conv_key = ?1",
                    [pni_key.encode()],
                    |r| r.get(0),
                )
                .unwrap(),
        );
        let first = match enqueue_files(
            h.connection_mut(),
            &net,
            old_conv,
            files,
            Some(Text::plain("the caption")),
            None,
            1_000,
        ) {
            Ok(FilesStep::More(send)) => send,
            other => panic!("not a part: {other:?}"),
        };
        h.apply(vec![BackendEvent::IdentityMerged {
            from: pni,
            into: alice,
        }])
        .unwrap();
        let conn = h.connection();
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        assert_ne!(conv, old_conv, "the chat the send was started in is gone");
        assert_eq!(
            id_of(
                conn,
                "SELECT count(*) FROM outbox o JOIN messages m ON m.id = o.message_id
                 WHERE o.state = 'Staged' AND o.conversation_id = m.conversation_id"
            ),
            i64::try_from(ENQUEUE_CHUNK).unwrap(),
            "the merge moved the stored part"
        );
        assert_eq!(
            id_of(conn, "SELECT count(DISTINCT conversation_id) FROM outbox"),
            1
        );
        (h, net, first, conv)
    }

    /// K-A fix C6: a refusal as a first contact also fails the rows of the chat that are still
    /// being stored (`Staged`), not only the `Queued` ones: they would be due a moment later.
    #[test]
    fn a_reach_out_refusal_also_fails_the_rows_still_being_staged() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::remote;

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![BackendEvent::Message(remote(
            &key, &alice, "100", 100, "hello",
        ))])
        .unwrap();
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("staged-463", vec![UserId::parse(ME).unwrap()]),
        );
        let conv = ConversationId(id_of(h.connection(), "SELECT id FROM conversations"));
        for (i, text) in ["refused", "queued", "staging"].into_iter().enumerate() {
            enqueue_text(
                h.connection_mut(),
                &net,
                conv,
                Text::plain(text),
                None,
                1_000 + i64::try_from(i).unwrap(),
            )
            .unwrap();
        }
        // The third is the first part of a send that is still being written.
        h.connection()
            .execute(
                "UPDATE outbox SET state = ?1 WHERE id = (SELECT max(id) FROM outbox)",
                [STAGED],
            )
            .unwrap();
        let first = MessageId(id_of(
            h.connection(),
            "SELECT message_id FROM outbox ORDER BY id LIMIT 1",
        ));
        let account = h.account();
        {
            let conn = h.connection_mut();
            let tx = conn.transaction().unwrap();
            let mut apply = apply_for(&tx, account, MsgIdScope::AccountAuthor, 2_000).unwrap();
            fail_waiting_of(&tx, &mut apply, conv, first, &FailReason::ReachOutRefused).unwrap();
            apply.finish().unwrap();
            drop(apply);
            tx.commit().unwrap();
        }
        let states: Vec<String> = h
            .connection()
            .prepare("SELECT state FROM outbox ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            states,
            [QUEUED, FAILED, FAILED],
            "the refused row is the caller's; the queued and the staged one fail"
        );
    }

    /// `n` small files as the import step hands them to a send.
    fn files_of(n: usize) -> std::sync::Arc<[crate::attachments::import::Imported]> {
        use crate::attachments::import::{Imported, Written};
        use kourier_model::AttachmentKind;
        (0..n)
            .map(|i| Imported {
                kind: AttachmentKind::File,
                content: AttachmentKind::File,
                file_name: Some(format!("doc-{i:03}.txt")),
                dims: None,
                blurhash: None,
                file: Written {
                    path: format!("core/aa/{i:032x}").into(),
                    key: kourier_blob::FileKey::from_bytes([1; 32]),
                    size: 10,
                    mime: "text/plain".into(),
                },
                thumb: None,
            })
            .collect()
    }

    /// Stores a send of `n` files in parts until its last part makes its rows due (or fails
    /// them); `between` runs after the first part.
    fn store_in_parts(
        h: &mut crate::testing::IngestHarness,
        net: &kourier_fake::FakeNetwork,
        conv: ConversationId,
        n: usize,
        now: i64,
        between: impl FnOnce(&mut crate::testing::IngestHarness),
    ) -> Vec<MessageId> {
        let mut between = Some(between);
        let mut step = enqueue_files(h.connection_mut(), net, conv, files_of(n), None, None, now)
            .map_err(|e| format!("{e:?}"))
            .unwrap();
        loop {
            match step {
                FilesStep::Done(_, messages) => return messages,
                FilesStep::More(send) => {
                    if let Some(f) = between.take() {
                        f(h);
                    }
                    step = enqueue_files_more(h.connection_mut(), net, send, now).unwrap();
                }
            }
        }
    }

    /// The states of the outbox rows of `messages`, in order.
    fn row_states(conn: &Connection, messages: &[MessageId]) -> Vec<String> {
        messages
            .iter()
            .map(|m| {
                conn.query_row(
                    "SELECT state FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
                    [m.0],
                    |r| r.get(0),
                )
                .unwrap()
            })
            .collect()
    }

    /// K-A fix 2 (C6, gap b): the parts of a large send that are stored after a 463 landed
    /// fail with the rest of the send; they do not turn due at its end. A send begun after the
    /// refusal is the user's own new act and goes out, though the chat has a refused message
    /// behind it.
    #[test]
    fn a_reach_out_refusal_between_the_parts_of_a_large_send_fails_the_parts_that_follow() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::remote;

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![BackendEvent::Message(remote(
            &key, &alice, "100", 100, "hello",
        ))])
        .unwrap();
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("parts-463", vec![UserId::parse(ME).unwrap()]),
        );
        let conv = ConversationId(id_of(h.connection(), "SELECT id FROM conversations"));
        // The message the network refuses (its answer lands while the send is stored).
        enqueue_text(
            h.connection_mut(),
            &net,
            conv,
            Text::plain("refused"),
            None,
            900,
        )
        .unwrap();
        let refused = MessageId(id_of(h.connection(), "SELECT message_id FROM outbox"));
        let account = h.account();
        let n = 2 * ENQUEUE_CHUNK + 22;
        let send = store_in_parts(&mut h, &net, conv, n, 1_000, |h| {
            let conn = h.connection_mut();
            let tx = conn.transaction().unwrap();
            let mut apply = apply_for(&tx, account, MsgIdScope::AccountAuthor, 1_500).unwrap();
            fail_waiting_of(&tx, &mut apply, conv, refused, &FailReason::ReachOutRefused).unwrap();
            apply.finish().unwrap();
            drop(apply);
            tx.commit().unwrap();
        });
        assert_eq!(send.len(), n);
        let states = row_states(h.connection(), &send);
        assert!(
            states.iter().all(|s| s == FAILED),
            "the part stored before the refusal and every part after it fail: {states:?}"
        );
        for m in [send[0], send[n - 1]] {
            assert_eq!(
                floor(h.connection(), m).unwrap(),
                Some(OutgoingState::Failed {
                    reason: FailReason::ReachOutRefused,
                    retryable: false
                }),
                "shown as the 463 line"
            );
        }
        // Only the row the refusal was answered for waits for its own answer.
        let due = take_due(
            h.connection_mut(),
            account,
            NetRules::signal(MsgIdScope::AccountAuthor),
            3_000,
            &Skip::default(),
            4,
        )
        .unwrap();
        assert_eq!(
            due.attempts.iter().map(|a| a.message).collect::<Vec<_>>(),
            [refused],
            "no part of the refused send is attempted"
        );

        // A send begun afterwards: the chat's newest floors are the refusal, and it is queued.
        let later = store_in_parts(&mut h, &net, conv, ENQUEUE_CHUNK + 3, 5_000, |_| {});
        let states = row_states(h.connection(), &later);
        assert!(
            states.iter().all(|s| s == QUEUED),
            "a send begun after the refusal is due: {states:?}"
        );
    }

    /// K-A fix 2 (C4): what a hold's time is given back to. Rows that waited through the whole
    /// hold get all of it, a row made during the hold only the time since it was made, and a
    /// row that is not waiting (failed) nothing; a hold without a start (a row an older build
    /// wrote) gives nothing back.
    #[test]
    fn the_time_a_hold_stood_is_given_back_to_the_rows_that_waited_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![kourier_backend_api::BackendEvent::Message(
            kourier_fake::scripts::remote(&key, &alice, "100", 100, "hello"),
        )])
        .unwrap();
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("give-back", vec![UserId::parse(ME).unwrap()]),
        );
        let conv = ConversationId(id_of(h.connection(), "SELECT id FROM conversations"));
        let hour = 3_600_000_i64;
        let day = ms(BUDGET);
        // Made before the hold (t = 0), made during it (t = 3 h), and one that already failed.
        for (text, at) in [("before", 0), ("during", 3 * hour), ("failed", 0)] {
            enqueue_text(h.connection_mut(), &net, conv, Text::plain(text), None, at).unwrap();
        }
        h.connection()
            .execute(
                "UPDATE outbox SET state = ?1 WHERE id = (SELECT max(id) FROM outbox)",
                [FAILED],
            )
            .unwrap();
        let deadlines = |h: &crate::testing::IngestHarness| -> Vec<i64> {
            h.connection()
                .prepare("SELECT deadline_at FROM outbox ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert_eq!(deadlines(&h), [day, 3 * hour + day, day]);
        let account = h.account();
        let hold = |since_ms, until_ms| StoredHold {
            hold: AccountHold {
                until_ms,
                reason: kourier_model::HoldReason::ReachOutLocked,
            },
            since_ms,
        };
        // No start: nothing to give back.
        assert_eq!(
            give_back_held_time(
                h.connection(),
                account,
                &hold(None, None),
                30 * hour,
                OutboxPolicy::SIGNAL
            )
            .unwrap(),
            0
        );
        assert_eq!(deadlines(&h), [day, 3 * hour + day, day]);
        // The hold began at 1 h and the backend lifted it at 30 h.
        let rows = give_back_held_time(
            h.connection(),
            account,
            &hold(Some(hour), None),
            30 * hour,
            OutboxPolicy::SIGNAL,
        )
        .unwrap();
        assert_eq!(rows, 2);
        assert_eq!(
            deadlines(&h),
            [day + 29 * hour, 3 * hour + day + 27 * hour, day],
            "the whole hold for the first row, the time since it was made for the second, none for the failed one"
        );
        // A second hold, from 31 h, ran out at its date (33 h) and the core looked at 40 h:
        // it is given back up to its date.
        let before = deadlines(&h);
        give_back_held_time(
            h.connection(),
            account,
            &hold(Some(31 * hour), Some(u64::try_from(33 * hour).unwrap())),
            40 * hour,
            OutboxPolicy::SIGNAL,
        )
        .unwrap();
        let after = deadlines(&h);
        assert_eq!(after[0] - before[0], 2 * hour, "until 33 h, not 40 h");
        assert_eq!(after[1] - before[1], 2 * hour);
        assert_eq!(after[2], before[2]);
    }

    /// A send that is stored in parts goes on where its messages are after a merge of two
    /// chats: the parts that follow are written into the chat that stayed, and the last one
    /// makes the rows of this send due, not the `Staged` rows of another send that is being
    /// stored in that chat.
    #[test]
    fn a_large_send_goes_on_in_the_chat_its_chat_was_merged_into() {
        let dir = tempfile::tempdir().unwrap();
        let (mut h, net, first, conv) = a_part_whose_chat_was_merged(&dir);
        let account = h.account();
        let conn = h.connection_mut();
        // Another send is being stored in the chat that stayed (its own compose queue).
        let other = MessageId(id_of(
            conn,
            "SELECT id FROM messages WHERE net_local = '100'",
        ));
        conn.execute(
            "INSERT INTO outbox(message_id, conversation_id, kind, request, state, attempts,
               deadline_at) VALUES (?1, ?2, 'Message', '{}', 'Staged', 0, 9000000)",
            (other.0, conv.0),
        )
        .unwrap();
        let mut step = enqueue_files_more(conn, &net, first, 2_000);
        let (written, messages) = loop {
            match step {
                Ok(FilesStep::Done(written, messages)) => break (written, messages),
                Ok(FilesStep::More(send)) => {
                    assert_eq!(
                        id_of(conn, "SELECT count(*) FROM outbox WHERE state = 'Queued'"),
                        0
                    );
                    step = enqueue_files_more(conn, &net, send, 2_000);
                }
                Err(e) => panic!("the send did not follow the merge: {e:?}"),
            }
        };
        let n = 2 * ENQUEUE_CHUNK + 22;
        assert_eq!(written.conv, conv);
        assert_eq!(messages.len(), n);
        assert_eq!(
            id_of(
                conn,
                "SELECT count(*) FROM outbox o JOIN messages m ON m.id = o.message_id
                 WHERE o.state = 'Queued' AND o.conversation_id = m.conversation_id"
            ),
            i64::try_from(n).unwrap(),
            "the whole send is due, in the chat that stayed"
        );
        assert_eq!(
            conn.query_row(
                "SELECT state FROM outbox WHERE message_id = ?1",
                [other.0],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "Staged",
            "the rows of another send are not this send's to make due"
        );
        conn.execute("DELETE FROM outbox WHERE message_id = ?1", [other.0])
            .unwrap();
        let due = take_due(
            conn,
            account,
            NetRules::signal(MsgIdScope::AccountAuthor),
            3_000,
            &Skip::default(),
            4,
        )
        .unwrap();
        assert_eq!(due.attempts.len(), 1);
        assert_eq!(due.attempts[0].message, messages[0]);
    }

    /// A send in parts that fails after its chat was merged into another one is dropped
    /// whole all the same: its rows are found by their messages, in the chat that stayed.
    #[test]
    fn a_large_send_that_fails_after_a_merge_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut h, _net, first, conv) = a_part_whose_chat_was_merged(&dir);
        let stored = first.stored().to_vec();
        drop(first);
        let conn = h.connection_mut();
        let dropped = discard_staged(conn, MsgIdScope::AccountAuthor, &stored, 2_000).unwrap();
        assert_eq!(
            dropped.unlink.len(),
            ENQUEUE_CHUNK,
            "its blobs are unlinked"
        );
        assert!(dropped.timelines.contains(&conv));
        assert_eq!(id_of(conn, "SELECT count(*) FROM outbox"), 0);
        assert_eq!(id_of(conn, "SELECT count(*) FROM files"), 0);
        assert_eq!(id_of(conn, "SELECT count(*) FROM attachments"), 0);
        assert_eq!(
            id_of(conn, "SELECT count(*) FROM messages"),
            2,
            "what alice sent under both ids"
        );
    }

    /// A challenge that ends by itself holds rows until then, whenever the core gets to
    /// look (the machine slept through its end, Kourier was closed): a row whose budget
    /// ended while the challenge stood gives up, one whose budget ended after it had run
    /// out keeps its late attempt. A wait without an end of its own counts up to the look.
    #[test]
    fn a_wait_for_a_challenge_counts_up_to_its_own_end() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::own;

        let dir = tempfile::tempdir().unwrap();
        let key = ConvKey::Direct(UserId::parse(ALICE).unwrap());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![
            BackendEvent::Message(own(&key, "300", 300, "ran out under the challenge")),
            BackendEvent::Message(own(&key, "301", 301, "ran out after it")),
        ])
        .unwrap();
        let account = h.account();
        let conn = h.connection_mut();
        let conv = id_of(conn, "SELECT id FROM conversations");
        for (id, local, deadline) in [(5, "300", 5_000), (6, "301", 20_000)] {
            conn.execute(
                "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state,
                   attempts, deadline_at)
                 SELECT ?1, id, ?2, 'Message', '{}', 'Queued', 1, ?4 FROM messages
                 WHERE net_local = ?3",
                (id, conv, local, deadline),
            )
            .unwrap();
        }
        let state = |conn: &Connection, id: i64| -> String {
            conn.query_row("SELECT state FROM outbox WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };
        let scope = MsgIdScope::AccountAuthor;
        // The challenge came at 1,000 and ends by itself at 10,000.
        let timed = [Wait {
            conv: None,
            since: 1_000,
            until: Some(10_000),
        }];
        let gave = give_up_overdue(conn, account, scope, 2_000, &timed).unwrap();
        assert_eq!(
            (state(conn, 5), state(conn, 6)),
            (QUEUED.into(), QUEUED.into())
        );
        assert_eq!(
            gave.next_deadline,
            Some(5_000),
            "the one budget that ends under this challenge"
        );
        // The core looks again long after both budgets and the challenge ended.
        let gave = give_up_overdue(conn, account, scope, 50_000, &timed).unwrap();
        assert_eq!(
            (state(conn, 5), state(conn, 6)),
            (FAILED.into(), QUEUED.into()),
            "only the budget that ended while the challenge stood is used up"
        );
        assert_eq!(gave.next_deadline, None);
        assert_eq!(
            gave.changes
                .messages
                .values()
                .map(|m| m.len())
                .sum::<usize>(),
            1
        );
        // A challenge that waits for the user alone holds rows up to the look.
        let open = [Wait {
            conv: None,
            since: 1_000,
            until: None,
        }];
        give_up_overdue(conn, account, scope, 50_000, &open).unwrap();
        assert_eq!(state(conn, 6), FAILED);
    }

    /// The challenge an attempt was refused with is stored by the transaction that records
    /// the attempt: there is no moment at which the row waits and a restart would not know
    /// why. A stored challenge reads back as it was written and is dropped after a day.
    #[test]
    fn a_challenge_is_stored_with_the_answer_that_brought_it() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::own;

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![BackendEvent::Message(own(&key, "300", 300, "held"))])
            .unwrap();
        let account = h.account();
        let conn = h.connection_mut();
        let message = MessageId(id_of(conn, "SELECT id FROM messages"));
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        conn.execute(
            "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
               deadline_at) VALUES (5, ?1, ?2, 'Message', '{}', 'Transmitting', 1, 90000000)",
            (message.0, conv.0),
        )
        .unwrap();
        assert_eq!(stored_challenge(conn, account).unwrap(), None);
        let refused = TransmitOutcome::Failed(BackendError::new(
            ErrorKind::Challenge {
                token: Redacted::new("token-1".into()),
                url: "https://example.invalid/c".into(),
                retry_after: Some(Duration::from_secs(90)),
            },
            "challenge required",
        ));
        let a = attempt(&key, conv, message, text("held"));
        let recorded = record_outcome(
            conn,
            account,
            NetRules::signal(MsgIdScope::AccountAuthor),
            1_000,
            &a,
            Some(refused),
        )
        .unwrap();
        assert!(matches!(
            recorded.pause,
            Some(Pause::Account { challenge: Some(_) })
        ));
        let stored = stored_challenge(conn, account).unwrap().expect("stored");
        assert_eq!(
            stored,
            StoredChallenge {
                url: "https://example.invalid/c".into(),
                token: Some(Redacted::new("token-1".into())),
                retry_at: Some(91_000),
                since: 1_000,
            }
        );
        let policy = OutboxPolicy::SIGNAL;
        assert!(stored.is_fresh(1_000, policy) && stored.is_fresh(1_000 + ms(BUDGET), policy));
        assert!(
            !stored.is_fresh(1_001 + ms(BUDGET), policy),
            "a day later it is over"
        );
        assert!(
            !stored.is_fresh(999, policy),
            "from a clock that was set back since"
        );
        assert_eq!(
            id_of(conn, "SELECT count(*) FROM outbox WHERE state = 'Queued'"),
            1,
            "the row waits"
        );
        store_challenge(conn, account, None).unwrap();
        assert_eq!(stored_challenge(conn, account).unwrap(), None);
        // A value this build cannot read is no challenge.
        conn.execute(
            "INSERT INTO settings(scope, key, value) VALUES (?1, 'challenge', 'not json')",
            [alloc_scope(account)],
        )
        .unwrap();
        assert_eq!(stored_challenge(conn, account).unwrap(), None);
    }

    const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
    const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
    const ALICE_PNI: &str = "pni:00000000-0000-4000-8000-0000000a11ce";

    fn harness(dir: &tempfile::TempDir, scope: MsgIdScope) -> crate::testing::IngestHarness {
        crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([7u8; 32]),
            NetworkKind::Fake,
            scope,
            &[UserId::parse(ME).unwrap()],
        )
        .unwrap()
    }

    fn id_of(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// T6b, the second half: on a network whose ids are unique per sender in a chat a call
    /// line holds its id in its chat like an own message does, so an own send is never given
    /// it (the line is a row under the per-conversation key, which the per-sender key does
    /// not see: `Apply::by_key` answers an own key with the line of the chat).
    #[test]
    fn an_own_send_is_never_given_the_id_of_a_call_line_under_per_sender_ids() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::calls::Call;
        use kourier_model::CallMedia;

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let at: i64 = 1_700_000_000_000;
        let flavor = kourier_fake::Flavor::WhatsAppLike;
        // The id the allocator tries first at `at`; a line of the chat has it.
        let first = flavor.allocate(u64::try_from(at).unwrap()).local;
        let BackendEvent::Message(mut ring) = Call::direct(&alice, 7, CallMedia::Voice).ring(at)
        else {
            panic!("a ring is a message");
        };
        ring.local = first.clone();
        let mut h = harness(&dir, MsgIdScope::ConversationAuthor);
        h.set_now(at);
        h.apply(vec![BackendEvent::Message(ring)]).unwrap();
        assert_eq!(
            id_of(
                h.connection(),
                "SELECT count(*) FROM messages WHERE net_scope = 1"
            ),
            1,
            "the line is there under the id"
        );
        let conv = ConversationId(id_of(h.connection(), "SELECT id FROM conversations"));
        let account = h.account();
        let net = kourier_fake::FakeNetwork::new(
            flavor,
            kourier_fake::Script::new("alloc", vec![UserId::parse(ME).unwrap()]),
        );
        let conn = h.connection_mut();
        let tx = conn.transaction().unwrap();
        let apply = apply_for(&tx, account, MsgIdScope::ConversationAuthor, at).unwrap();
        let id = allocate(&apply, &net, conv, &key, at).unwrap();
        assert_ne!(id.local, first, "the line's id was skipped");
        assert!(
            apply.by_key(conv, "", &id.local).unwrap().is_none(),
            "and the id it got is free"
        );
    }

    /// The attempt that sends `kind` for `message` from outbox row 5.
    fn attempt(
        key: &ConvKey,
        conv: ConversationId,
        message: MessageId,
        kind: OutgoingKind,
    ) -> Attempt {
        Attempt {
            outbox: 5,
            conv,
            message,
            req: OutgoingRequest {
                conv: key.clone(),
                local: "300".into(),
                sent_at: 300,
                kind,
                only: None,
                is_recipient_update: false,
            },
            attachments: Vec::new(),
            number: 1,
            started_at: 300,
        }
    }

    fn text(body: &str) -> OutgoingKind {
        OutgoingKind::Message {
            body: Body::text(body),
            reply_to: None,
            expiry: None,
        }
    }

    /// "Accepted for `user`".
    fn accepted(user: &UserId) -> Option<TransmitOutcome> {
        Some(TransmitOutcome::Sent {
            server_at: Some(400),
            per_recipient: vec![(user.clone(), RecipientState::Sent { sealed: true })],
        })
    }

    /// The answer to an attempt whose row is gone is recorded for nothing else: an accepted
    /// reaction is not an accepted message, and the answer to a send that an echo finished
    /// counts for its message only, whatever other rows are in flight.
    #[test]
    fn an_answer_whose_row_is_gone_touches_nothing_else() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::own;

        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        h.apply(vec![
            BackendEvent::Message(own(&key, "100", 100, "first")),
            BackendEvent::Message(own(&key, "200", 200, "second")),
        ])
        .unwrap();
        let account = h.account();
        let conn = h.connection_mut();
        let first = MessageId(id_of(
            conn,
            "SELECT id FROM messages WHERE net_local = '100'",
        ));
        let second = MessageId(id_of(
            conn,
            "SELECT id FROM messages WHERE net_local = '200'",
        ));
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        let record = |conn: &mut Connection, a: &Attempt| {
            let (scope, answer) = (MsgIdScope::AccountAuthor, accepted(&alice));
            record_outcome(conn, account, NetRules::signal(scope), 1_000, a, answer).unwrap()
        };

        // A reaction to `first` was in flight when its row went.
        let react = OutgoingKind::React {
            target: MsgTarget::to_id(Some(key.clone()), &NetMsgId::own("100")),
            emoji: "👍".into(),
            remove: false,
        };
        let before = send_states(conn, first).unwrap();
        let recorded = record(conn, &attempt(&key, conv, first, react));
        assert!(recorded.failed.is_none() && recorded.pause.is_none());
        assert_eq!(
            send_states(conn, first).unwrap(),
            before,
            "an accepted reaction is not an accepted message"
        );

        // The send of `first` was in flight when an echo finished its row; `second` is in
        // flight in row 6.
        conn.execute(
            "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
               deadline_at) VALUES (6, ?1, ?2, 'Message', '{}', 'Transmitting', 1, 9000)",
            (second.0, conv.0),
        )
        .unwrap();
        let before = send_states(conn, second).unwrap();
        record(conn, &attempt(&key, conv, first, text("first")));
        let row: (i64, String, i64) = conn
            .query_row("SELECT message_id, state, attempts FROM outbox", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(
            row,
            (second.0, TRANSMITTING.to_owned(), 1),
            "still in flight"
        );
        assert_eq!(send_states(conn, second).unwrap(), before);
        let got: Vec<RecipientState> = send_states(conn, first).unwrap().into_values().collect();
        assert_eq!(
            got,
            [RecipientState::Sent { sealed: true }],
            "recorded for the message it was sent for"
        );
    }

    /// A receipt can overtake the answer of its own transmit. From a recipient the
    /// conversation's member list does not name, it waits (§4.9) until that answer says who
    /// the message went to; the answer applies it in its own transaction. A receipt from
    /// someone the answer does not name keeps waiting.
    #[test]
    fn the_answer_of_a_send_lets_a_waiting_receipt_of_a_recipient_count() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::{own, target};
        use kourier_model::{Member, MemberList, MemberRole, ReceiptKind};

        let dir = tempfile::tempdir().unwrap();
        let (alice, bob, me) = (
            UserId::parse(ALICE).unwrap(),
            UserId::parse("aci:00000000-0000-4000-8000-000000000b0b").unwrap(),
            UserId::parse(ME).unwrap(),
        );
        let stranger = UserId::parse("aci:00000000-0000-4000-8000-00000000bad0").unwrap();
        let key = ConvKey::Group("gv2:b3V0Y29tZS1yZWNlaXB0".into());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let mut patch = kourier_model::ConversationPatch::new(key.clone());
        patch.members = Some(MemberList::snapshot(
            [&me, &alice]
                .into_iter()
                .map(|u| Member {
                    user: u.clone(),
                    role: MemberRole::Member,
                })
                .collect(),
        ));
        let receipt = |by: &UserId| BackendEvent::Receipts {
            targets: vec![target(None, None, "300")],
            by: by.clone(),
            kind: ReceiptKind::Delivered,
            at: 350,
        };
        h.apply(vec![
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(own(&key, "300", 300, "to the group")),
            // Bob got it (the server fans out to the group as it is now) and said so
            // before our send was answered; a stranger claims the same.
            receipt(&bob),
            receipt(&stranger),
        ])
        .unwrap();
        let account = h.account();
        let conn = h.connection_mut();
        let message = MessageId(id_of(conn, "SELECT id FROM messages"));
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        let waiting = |conn: &Connection| {
            (
                id_of(conn, "SELECT count(*) FROM receipts"),
                id_of(conn, "SELECT count(*) FROM pending_modifiers"),
            )
        };
        assert_eq!(waiting(conn), (0, 2));
        conn.execute(
            "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
               deadline_at) VALUES (5, ?1, ?2, 'Message', '{}', 'Transmitting', 1, 9000)",
            (message.0, conv.0),
        )
        .unwrap();
        let answer = Some(TransmitOutcome::Sent {
            server_at: Some(400),
            per_recipient: vec![
                (alice.clone(), RecipientState::Sent { sealed: true }),
                (bob.clone(), RecipientState::Sent { sealed: true }),
            ],
        });
        let a = attempt(&key, conv, message, text("to the group"));
        let recorded = record_outcome(
            conn,
            account,
            NetRules::signal(MsgIdScope::AccountAuthor),
            1_000,
            &a,
            answer,
        )
        .unwrap();
        assert!(recorded.failed.is_none());
        assert_eq!(waiting(conn), (1, 1), "bob's counts, the stranger's waits");
        let by: String = conn
            .query_row("SELECT user_id FROM receipts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(by, bob.as_str());
        assert!(
            recorded
                .changes
                .messages
                .get(&conv)
                .is_some_and(|m| m.contains(&message)),
            "the row is read again"
        );
    }

    /// A merge of two conversations (§3.3) can move an outbox row, and under per-conversation
    /// ids its message, while the transmit is in flight. The answer is recorded where the row
    /// is now, so it does not stay `Transmitting` and hold the conversation's queue.
    #[test]
    fn an_answer_follows_a_row_that_a_merge_moved() {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::own;

        let dir = tempfile::tempdir().unwrap();
        let (alice, pni) = (
            UserId::parse(ALICE).unwrap(),
            UserId::parse(ALICE_PNI).unwrap(),
        );
        let (key, pni_key) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(pni.clone()));
        let mut h = harness(&dir, MsgIdScope::Conversation);
        // The same own message under both identities of alice; the older row is kept.
        h.apply(vec![BackendEvent::Message(own(&key, "100", 100, "hello"))])
            .unwrap();
        h.apply(vec![BackendEvent::Message(own(
            &pni_key, "100", 100, "hello",
        ))])
        .unwrap();
        let account = h.account();
        let (dropped, old_conv) = h
            .connection()
            .query_row(
                "SELECT m.id, c.id FROM messages m JOIN conversations c
                   ON c.id = m.conversation_id WHERE c.conv_key = ?1",
                [pni_key.encode()],
                |r| Ok((MessageId(r.get(0)?), ConversationId(r.get(1)?))),
            )
            .unwrap();
        h.connection()
            .execute(
                "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state,
                   attempts, deadline_at) VALUES (5, ?1, ?2, 'Message', '{}', 'Transmitting', 1, 9000)",
                (dropped.0, old_conv.0),
            )
            .unwrap();
        h.apply(vec![BackendEvent::IdentityMerged {
            from: pni.clone(),
            into: alice,
        }])
        .unwrap();
        let conn = h.connection_mut();
        let kept = MessageId(id_of(conn, "SELECT id FROM messages"));
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        assert_ne!((kept, conv), (dropped, old_conv), "the merge moved the row");
        assert_eq!(
            id_of(conn, "SELECT message_id FROM outbox WHERE id = 5"),
            kept.0
        );

        // The attempt still names the row's old message and conversation.
        let stale = attempt(&pni_key, old_conv, dropped, text("hello"));
        let scope = MsgIdScope::Conversation;
        let recorded = record_outcome(
            conn,
            account,
            NetRules::signal(scope),
            1_000,
            &stale,
            accepted(&pni),
        )
        .unwrap();
        assert_eq!(id_of(conn, "SELECT count(*) FROM outbox"), 0, "done");
        let got: Vec<RecipientState> = send_states(conn, kept).unwrap().into_values().collect();
        assert_eq!(got, [RecipientState::Sent { sealed: true }]);
        assert!(recorded.changes.conversations.contains(&conv));
        assert!(!recorded.changes.conversations.contains(&old_conv));
    }

    /// The wait of a released row follows the cap of the account's policy (K-B1 B6): a row
    /// with ten attempts waits a minute under Signal's 15 minute cap and five seconds under a
    /// cap of five.
    #[test]
    fn release_waits_no_longer_than_the_cap_of_the_policy() {
        let dir = tempfile::tempdir().unwrap();
        let key = kourier_db::DbKey::from([7u8; 32]);
        let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).unwrap();
        kourier_db::migrate(&mut conn, &crate::store::MIGRATIONS).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
        conn.execute(
            "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
               deadline_at) VALUES (1, 1, 1, 'Message', '{}', 'Transmitting', 10, 0),
                                   (2, 2, 1, 'Message', '{}', 'Transmitting', 10, 0)",
            [],
        )
        .unwrap();
        let capped = OutboxPolicy {
            max_backoff: Duration::from_secs(5),
            ..OutboxPolicy::SIGNAL
        };
        let rules = |outbox| NetRules {
            outbox,
            ..NetRules::signal(MsgIdScope::AccountAuthor)
        };
        let released = |conn: &mut Connection, outbox, row| {
            release(conn, AccountId(1), rules(outbox), row, 1_000)
                .unwrap()
                .released
        };
        assert!(released(&mut conn, OutboxPolicy::SIGNAL, 1));
        assert!(released(&mut conn, capped, 2));
        let next = |id: i64| -> i64 {
            conn.query_row(
                "SELECT next_attempt_at FROM outbox WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(next(1), 1_000 + ms(backoff(11)), "Signal's curve");
        assert!(next(1) > 1_000 + 5_000, "a minute, not five seconds");
        assert_eq!(next(2), 1_000 + 5_000, "the policy's cap");
    }

    /// The back-off keeps its curve for every cap and only moves where it stops.
    #[test]
    fn backoff_capped_keeps_the_curve_and_moves_the_cap() {
        for n in [0, 1, 2, 5, 12, 40, u32::MAX] {
            assert_eq!(backoff_capped(n, MAX_BACKOFF), backoff(n), "attempt {n}");
        }
        let one = Duration::from_secs(1);
        assert_eq!(backoff_capped(4, one), Duration::from_millis(685));
        assert_eq!(backoff_capped(5, one), one, "1,303 ms is capped");
        assert_eq!(backoff_capped(u32::MAX, one), one);
        assert_eq!(
            backoff_capped(40, Duration::from_secs(3600)),
            Duration::from_secs(3600),
            "a higher cap lifts the old one"
        );
    }

    /// A stored challenge counts for the lifetime the network's policy states.
    #[test]
    fn a_stored_challenge_is_fresh_for_the_lifetime_of_the_policy() {
        let stored = StoredChallenge {
            url: "https://example.invalid/c".into(),
            token: None,
            retry_at: None,
            since: 1_000,
        };
        let short = OutboxPolicy {
            challenge_lifetime: Duration::from_secs(3600),
            ..OutboxPolicy::SIGNAL
        };
        assert!(stored.is_fresh(1_000 + 3_600_000, short));
        assert!(
            !stored.is_fresh(1_001 + 3_600_000, short),
            "an hour is over"
        );
        assert!(
            stored.is_fresh(1_001 + 3_600_000, OutboxPolicy::SIGNAL),
            "Signal's is a day"
        );
    }

    #[test]
    fn backoff_matches_signal() {
        let ms: Vec<u128> = (1..=6).map(|n| backoff(n).as_millis()).collect();
        assert_eq!(ms, [0, 190, 361, 685, 1303, 2476]);
        assert_eq!(backoff(0), Duration::ZERO);
        assert_eq!(backoff(40), MAX_BACKOFF);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
        // About 111 attempts fit into the one-day budget (Signal's MAX_ATTEMPTS).
        let mut total = Duration::ZERO;
        let mut n = 0;
        while total < BUDGET {
            n += 1;
            total += backoff(n);
        }
        assert!((100..=120).contains(&n), "{n}");
    }

    /// An own message of `key` with a `Message` outbox row in flight (`row` 5, the first
    /// attempt) whose `cancel_state` is `mark`; its attempt, ready for [`record_outcome`].
    fn in_flight_with_mark(
        h: &mut crate::testing::IngestHarness,
        key: &ConvKey,
        local: &str,
        row: i64,
        mark: i64,
    ) -> (MessageId, ConversationId, Attempt) {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::own;

        h.apply(vec![BackendEvent::Message(own(key, local, 100, "picture"))])
            .unwrap();
        let conn = h.connection_mut();
        let message = MessageId(id_of(
            conn,
            &format!("SELECT id FROM messages WHERE net_local = '{local}'"),
        ));
        let conv = ConversationId(id_of(conn, "SELECT id FROM conversations"));
        let mut a = attempt(key, conv, message, text("picture"));
        a.outbox = row;
        // The stored request is the attempt's own, so `take_due` can read the row again.
        conn.execute(
            "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
               deadline_at, cancel_state) VALUES (?1, ?2, ?3, 'Message', ?5, 'Transmitting', 1,
               9000, ?4)",
            (row, message.0, conv.0, mark, sql::to_json(&a.req).unwrap()),
        )
        .unwrap();
        (message, conv, a)
    }

    fn mark_of(conn: &Connection, row: i64) -> Option<(String, i64)> {
        conn.query_row(
            "SELECT state, cancel_state FROM outbox WHERE id = ?1",
            [row],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
    }

    /// Upload progress, critic B2. What an attempt's end does to a message depends on what the
    /// backend promised: after `Stopping` (mark 2) nothing went out, so `Failed` AND no reply
    /// take the message back with its outbox row; a send that went out anyway is recorded as
    /// sent (a backend bug, the mark goes). A mark that was only ASKED (1) promises nothing:
    /// the outcome is read as it comes, and the row goes on without the mark.
    #[test]
    fn a_stopped_attempt_is_taken_back_and_an_asked_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let account = h.account();
        let scope = MsgIdScope::AccountAuthor;
        let transient = || {
            Some(TransmitOutcome::Failed(BackendError::new(
                ErrorKind::Transient { retry_after: None },
                "timed out",
            )))
        };
        let refused = || {
            Some(TransmitOutcome::Failed(BackendError::new(
                ErrorKind::Internal,
                "stopped by the user before anything was sent",
            )))
        };
        // (name, mark, outcome, taken back?)
        let cases: Vec<(&str, i64, Option<TransmitOutcome>, bool)> = vec![
            ("stopped, failed", CANCEL_STOPPING, refused(), true),
            (
                "stopped, a transient failure",
                CANCEL_STOPPING,
                transient(),
                true,
            ),
            ("stopped, no reply", CANCEL_STOPPING, None, true),
            (
                "asked, a transient failure",
                CANCEL_ASKED,
                transient(),
                false,
            ),
            ("asked, no reply", CANCEL_ASKED, None, false),
        ];
        for (n, (name, mark, outcome, taken_back)) in cases.into_iter().enumerate() {
            let local = format!("{}", 200 + n);
            let (message, conv, a) = in_flight_with_mark(&mut h, &key, &local, 10 + n as i64, mark);
            let conn = h.connection_mut();
            let rec =
                record_outcome(conn, account, NetRules::signal(scope), 1_000, &a, outcome).unwrap();
            let there = count_of(conn, "messages", message);
            if taken_back {
                assert_eq!(there, 0, "{name}: the message is gone");
                assert!(mark_of(conn, a.outbox).is_none(), "{name}: so is its row");
                assert!(rec.changes.timelines.contains(&conv), "{name}: published");
            } else {
                assert_eq!(there, 1, "{name}: the message stays");
                assert_eq!(
                    mark_of(conn, a.outbox).map(|(_, m)| m),
                    Some(CANCEL_NONE),
                    "{name}: the row goes on without the mark"
                );
            }
        }
        // A stopped attempt that reports a send went out: sent, never taken back.
        let (message, _, a) = in_flight_with_mark(&mut h, &key, "299", 99, CANCEL_STOPPING);
        let conn = h.connection_mut();
        record_outcome(
            conn,
            account,
            NetRules::signal(scope),
            1_000,
            &a,
            accepted(&alice),
        )
        .unwrap();
        assert_eq!(
            count_of(conn, "messages", message),
            1,
            "it was sent: it stays"
        );
        assert!(mark_of(conn, 99).is_none(), "its row ended with the send");
    }

    fn count_of(conn: &Connection, table: &str, message: MessageId) -> i64 {
        conn.query_row(
            &format!("SELECT count(*) FROM {table} WHERE id = ?1"),
            [message.0],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Upload progress, critic B2. A start after a quit or a crash in the middle of a stop:
    /// the row whose stop the backend had ACCEPTED (2) is taken back with the files'
    /// attachment rows, a row only asked (1) and one never asked (0) are queued again, all
    /// without a mark.
    #[test]
    fn a_start_takes_back_a_stopped_attempt_and_queues_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice);
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let account = h.account();
        let (m0, conv, _) = in_flight_with_mark(&mut h, &key, "300", 20, CANCEL_NONE);
        let (m1, _, _) = in_flight_with_mark(&mut h, &key, "301", 21, CANCEL_ASKED);
        let (m2, _, _) = in_flight_with_mark(&mut h, &key, "302", 22, CANCEL_STOPPING);
        let conn = h.connection_mut();
        // The stopped message has an attachment row, which goes with it.
        conn.execute(
            "INSERT INTO attachments(message_id, idx, kind, state)
             VALUES (?1, 0, 'Image', '\"Local\"')",
            [m2.0],
        )
        .unwrap();
        let (rows, changes) =
            reset_transmitting(conn, account, MsgIdScope::AccountAuthor, 5_000).unwrap();
        assert_eq!(rows, 3);
        assert_eq!(count_of(conn, "messages", m2), 0, "the stopped one is gone");
        assert!(mark_of(conn, 22).is_none());
        assert_eq!(
            id_of(conn, "SELECT count(*) FROM attachments"),
            0,
            "with its attachment rows"
        );
        assert!(changes.timelines.contains(&conv), "the chat is published");
        for (m, row) in [(m0, 20), (m1, 21)] {
            assert_eq!(count_of(conn, "messages", m), 1);
            assert_eq!(
                mark_of(conn, row),
                Some((QUEUED.to_owned(), CANCEL_NONE)),
                "queued again, without a mark"
            );
        }
    }

    /// Upload progress, fix K1 (the cancel review's HIGH 1). A stop mark never outlives the
    /// attempt it was written for. When an outcome cannot be recorded, `release` hands the row
    /// back: with the backend's `Stopping` (mark 2) the attempt reached nobody and the user
    /// asked for the message to go, so it is TAKEN BACK in the same write (rows, files by the
    /// returned changes) and no second attempt ever starts; a mark that was only asked (1)
    /// becomes 0, the second attempt is a normal one, and its no-reply outcome keeps the message.
    #[test]
    fn a_release_ends_the_stop_with_its_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice);
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let account = h.account();
        let rules = NetRules::signal(MsgIdScope::AccountAuthor);

        // Mark 2: the message goes with its attachment rows, published, and nothing starts.
        let (stopped, conv, _) = in_flight_with_mark(&mut h, &key, "600", 50, CANCEL_STOPPING);
        let conn = h.connection_mut();
        conn.execute(
            "INSERT INTO attachments(message_id, idx, kind, state)
             VALUES (?1, 0, 'Image', '\"Local\"')",
            [stopped.0],
        )
        .unwrap();
        let released = release(conn, account, rules, 50, 1_000).unwrap();
        assert!(released.released);
        assert!(released.changes.timelines.contains(&conv), "published");
        assert_eq!(count_of(conn, "messages", stopped), 0, "taken back");
        assert!(mark_of(conn, 50).is_none(), "with its row");
        assert_eq!(id_of(conn, "SELECT count(*) FROM attachments"), 0);
        let due = take_due(conn, account, rules, 3_000, &Skip::default(), 4).unwrap();
        assert!(due.attempts.is_empty(), "no second attempt: {due:?}");

        // Mark 1: queued again without the mark; attempt 2 is an ordinary attempt.
        let (asked, _, _) = in_flight_with_mark(&mut h, &key, "601", 51, CANCEL_ASKED);
        let conn = h.connection_mut();
        let released = release(conn, account, rules, 51, 1_000).unwrap();
        assert!(released.released && released.changes.timelines.is_empty());
        assert_eq!(
            mark_of(conn, 51),
            Some((QUEUED.to_owned(), CANCEL_NONE)),
            "the mark ends with the attempt"
        );
        assert_eq!(count_of(conn, "messages", asked), 1, "the message stays");
        let due = take_due(conn, account, rules, 3_000, &Skip::default(), 4).unwrap();
        let [second] = &due.attempts[..] else {
            panic!("{due:?}")
        };
        assert_eq!((second.message, second.number), (asked, 2));
        record_outcome(conn, account, rules, 3_001, second, None).unwrap();
        assert_eq!(
            count_of(conn, "messages", asked),
            1,
            "attempt 2 never asked to stop: a lost reply keeps the message"
        );
        assert_eq!(
            mark_of(conn, 51).map(|r| (r.0, r.1)),
            Some((QUEUED.to_owned(), CANCEL_NONE))
        );
    }

    /// Upload progress, fix K1, belt and braces. A mark that is on a row anyway (an older
    /// build, a write that went wrong) is cleared when `take_due` starts the row's next
    /// attempt, so it is never read as that attempt's: the lost reply of attempt 2 keeps the
    /// message in both cases.
    #[test]
    fn a_stale_stop_mark_is_cleared_when_the_next_attempt_starts() {
        for stale in [CANCEL_ASKED, CANCEL_STOPPING] {
            let dir = tempfile::tempdir().unwrap();
            let key = ConvKey::Direct(UserId::parse(ALICE).unwrap());
            let mut h = harness(&dir, MsgIdScope::AccountAuthor);
            let account = h.account();
            let rules = NetRules::signal(MsgIdScope::AccountAuthor);
            let (message, _, _) = in_flight_with_mark(&mut h, &key, "700", 60, CANCEL_NONE);
            let conn = h.connection_mut();
            // The row waits for its retry, with the mark written directly.
            conn.execute(
                "UPDATE outbox SET state = ?2, cancel_state = ?3 WHERE id = ?1",
                (60, QUEUED, stale),
            )
            .unwrap();
            assert_eq!(mark_of(conn, 60).map(|r| r.1), Some(stale));
            let due = take_due(conn, account, rules, 3_000, &Skip::default(), 4).unwrap();
            let [second] = &due.attempts[..] else {
                panic!("mark {stale}: {due:?}")
            };
            assert_eq!(second.number, 2);
            assert_eq!(
                mark_of(conn, 60),
                Some((TRANSMITTING.to_owned(), CANCEL_NONE)),
                "mark {stale}: cleared with the start"
            );
            record_outcome(conn, account, rules, 3_001, second, None).unwrap();
            assert_eq!(
                count_of(conn, "messages", message),
                1,
                "mark {stale}: a message that may have gone is never taken back"
            );
        }
    }

    /// The dispatcher's write of the backend's answer: `Stopping` makes the mark durable and
    /// only from "asked"; `TooLate` (or nothing) clears it; a row nobody asked about is left
    /// alone.
    #[test]
    fn the_answer_to_a_stop_is_written_from_asked_only() {
        let dir = tempfile::tempdir().unwrap();
        let key = ConvKey::Direct(UserId::parse(ALICE).unwrap());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let (m0, _, _) = in_flight_with_mark(&mut h, &key, "400", 30, CANCEL_NONE);
        let (m1, _, _) = in_flight_with_mark(&mut h, &key, "401", 31, CANCEL_ASKED);
        let (m2, _, _) = in_flight_with_mark(&mut h, &key, "402", 32, CANCEL_ASKED);
        let conn = h.connection_mut();
        assert!(!answer_cancel(conn, m0, true).unwrap(), "never asked");
        assert_eq!(mark_of(conn, 30).map(|r| r.1), Some(CANCEL_NONE));
        assert!(answer_cancel(conn, m1, true).unwrap());
        assert_eq!(mark_of(conn, 31).map(|r| r.1), Some(CANCEL_STOPPING));
        assert!(
            !answer_cancel(conn, m1, false).unwrap(),
            "a mark that is Stopping is never cleared by a later TooLate"
        );
        assert_eq!(mark_of(conn, 31).map(|r| r.1), Some(CANCEL_STOPPING));
        assert!(answer_cancel(conn, m2, false).unwrap());
        assert_eq!(mark_of(conn, 32).map(|r| r.1), Some(CANCEL_NONE));
    }

    /// Upload progress, fix K2 (the cancel review's LOW 2). A message that waits is taken back
    /// by the flag's own rule, judged inside the write: one nothing was attempted for goes; a
    /// retry wait (an attempt before may have reached the server: a dropped reply, a temporary
    /// failure) is refused `BeingSent` and stays with its row; one a recipient has stays too.
    #[test]
    fn a_cancel_of_a_waiting_message_follows_the_rule_of_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let key = ConvKey::Direct(UserId::parse(ALICE).unwrap());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let scope = MsgIdScope::AccountAuthor;
        let (retry, _, _) = in_flight_with_mark(&mut h, &key, "800", 70, CANCEL_NONE);
        let (fresh, _, _) = in_flight_with_mark(&mut h, &key, "801", 71, CANCEL_NONE);
        let conn = h.connection_mut();
        // 70 waits for its retry after one attempt; 71 was never attempted.
        conn.execute(
            "UPDATE outbox SET state = ?2, attempts = ?3 WHERE id = ?1",
            (70, QUEUED, 1),
        )
        .unwrap();
        conn.execute(
            "UPDATE outbox SET state = ?2, attempts = ?3 WHERE id = ?1",
            (71, QUEUED, 0),
        )
        .unwrap();
        match cancel(conn, scope, retry, 1_000, false) {
            Err(CoreError::Refused { reason, .. }) => assert_eq!(reason, Refusal::BeingSent),
            other => panic!("a retry wait is not taken back: {other:?}"),
        }
        assert_eq!(count_of(conn, "messages", retry), 1, "the message stays");
        assert_eq!(mark_of(conn, 70).map(|r| r.0), Some(QUEUED.to_owned()));
        match cancel(conn, scope, fresh, 1_000, false).unwrap() {
            Cancelled::TakenBack(_) => {}
            other => panic!("a message never attempted is taken back: {other:?}"),
        }
        assert_eq!(count_of(conn, "messages", fresh), 0, "gone");
        assert!(mark_of(conn, 71).is_none(), "with its row");
    }

    /// The command's half of the rule: an uploading first attempt is ASKED (the mark is
    /// written, the message stays) and only once; a message in flight that the rule does not
    /// allow (no reported upload, a retry, a recipient that has it) is refused as before.
    #[test]
    fn a_cancel_of_a_message_in_flight_asks_once_and_only_where_it_may() {
        let dir = tempfile::tempdir().unwrap();
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(&dir, MsgIdScope::AccountAuthor);
        let (message, conv, _) = in_flight_with_mark(&mut h, &key, "500", 40, CANCEL_NONE);
        let conn = h.connection_mut();
        let scope = MsgIdScope::AccountAuthor;
        let refusal = |r: Result<Cancelled>| match r {
            Err(CoreError::Refused { reason, .. }) => reason,
            other => panic!("not refused: {other:?}"),
        };
        // No upload reported.
        assert_eq!(
            refusal(cancel(conn, scope, message, 1_000, false)),
            Refusal::BeingSent
        );
        assert_eq!(mark_of(conn, 40).map(|r| r.1), Some(CANCEL_NONE));
        // A retry.
        conn.execute("UPDATE outbox SET attempts = 2 WHERE id = 40", [])
            .unwrap();
        assert_eq!(
            refusal(cancel(conn, scope, message, 1_000, true)),
            Refusal::BeingSent
        );
        conn.execute("UPDATE outbox SET attempts = 1 WHERE id = 40", [])
            .unwrap();
        // The first attempt of an uploading message: asked, the message stays, the chat's
        // timeline is published for the greyed X.
        match cancel(conn, scope, message, 1_000, true).unwrap() {
            Cancelled::Asked(w) => {
                assert_eq!(w.conv, conv);
                assert!(
                    w.changes
                        .messages
                        .get(&conv)
                        .is_some_and(|ids| ids.contains(&message)),
                    "the row is read again: the X greys out"
                );
            }
            other => panic!("not asked: {other:?}"),
        }
        assert_eq!(mark_of(conn, 40).map(|r| r.1), Some(CANCEL_ASKED));
        assert_eq!(count_of(conn, "messages", message), 1);
        // Asked once.
        assert_eq!(
            refusal(cancel(conn, scope, message, 1_000, true)),
            Refusal::BeingSent
        );
    }
    // ---------- the gate of a new send (G4) ----------

    /// Every reason the model has for "the user cannot send here". A new variant of
    /// `CannotSend` stops the exhaustive `slot` below from compiling until it is listed here.
    fn every_cannot_send() -> Vec<kourier_model::CannotSend> {
        use kourier_model::CannotSend::*;
        let all = vec![
            NotMember,
            AdminsOnly,
            Blocked,
            RequestPending,
            Unregistered,
            NotAccepted,
            StillLoading,
            InvitationPending,
            JoinRequestPending,
            Other("a reason in the network's words".into()),
            NotYetBuilt,
        ];
        let slot = |c: &kourier_model::CannotSend| match c {
            NotMember => 0,
            AdminsOnly => 1,
            Blocked => 2,
            RequestPending => 3,
            Unregistered => 4,
            NotAccepted => 5,
            StillLoading => 6,
            InvitationPending => 7,
            JoinRequestPending => 8,
            Other(_) => 9,
            NotYetBuilt => 10,
        };
        assert_eq!(
            all.iter().map(slot).collect::<Vec<_>>(),
            (0..11).collect::<Vec<_>>(),
            "every variant is listed once"
        );
        all
    }

    /// The backend states the features of the chat with Alice, `reason` being why we cannot
    /// send there (`None`: we can), as `ConversationUpsert` does.
    fn state_cannot_send(
        h: &mut crate::testing::IngestHarness,
        reason: Option<kourier_model::CannotSend>,
    ) {
        use kourier_backend_api::BackendEvent;
        let mut features = kourier_fake::scripts::signal_features(false);
        features.cannot_send = reason;
        let mut patch =
            kourier_model::ConversationPatch::new(ConvKey::Direct(UserId::parse(ALICE).unwrap()));
        patch.features = Some(features);
        h.apply(vec![BackendEvent::ConversationUpsert(patch)])
            .unwrap();
    }

    /// A chat with Alice that holds one message of hers and no stated features (`features`
    /// NULL), a draft, and a fake network to send through.
    fn a_chat_with_a_draft(
        dir: &tempfile::TempDir,
    ) -> (
        crate::testing::IngestHarness,
        kourier_fake::FakeNetwork,
        ConversationId,
    ) {
        use kourier_backend_api::BackendEvent;
        use kourier_fake::scripts::remote;
        let alice = UserId::parse(ALICE).unwrap();
        let key = ConvKey::Direct(alice.clone());
        let mut h = harness(dir, MsgIdScope::AccountAuthor);
        h.apply(vec![BackendEvent::Message(remote(
            &key, &alice, "100", 100, "hello",
        ))])
        .unwrap();
        let net = kourier_fake::FakeNetwork::new(
            kourier_fake::Flavor::SignalLike,
            kourier_fake::Script::new("gate", vec![UserId::parse(ME).unwrap()]),
        );
        let conv = ConversationId(id_of(h.connection(), "SELECT id FROM conversations"));
        save_draft(h.connection(), conv, Some(&Text::plain("my unsent draft"))).unwrap();
        (h, net, conv)
    }

    /// What a send stores: messages, outbox rows, `files` rows and attachment rows.
    fn stored_rows(conn: &Connection) -> (i64, i64, i64, i64) {
        conn.query_row(
            "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM outbox),
                    (SELECT count(*) FROM files), (SELECT count(*) FROM attachments)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    fn draft_stays(conn: &Connection, conv: ConversationId) -> bool {
        conn.query_row(
            "SELECT draft LIKE '%my unsent draft%' FROM conversations WHERE id = ?1",
            [conv.0],
            |r| r.get::<_, Option<bool>>(0),
        )
        .unwrap()
        .unwrap_or(false)
    }

    fn contact_card() -> kourier_model::ContactCard {
        kourier_model::ContactCard {
            name: Some("Synthetic Person".into()),
            ..Default::default()
        }
    }

    /// G4: a NEW send into a chat whose stored features say `cannot_send` is refused with
    /// `Refusal::CannotSend`, whatever the reason, for a text, for files with a caption and for
    /// a card: nothing is written (no message, outbox row, file or attachment row) and the
    /// draft stays. The composer is not there for such a chat; this is the core's own check
    /// against a stale UI, a second front-end and a race.
    #[test]
    fn a_new_send_into_a_chat_that_cannot_send_is_refused_and_stores_nothing() {
        for reason in every_cannot_send() {
            let dir = tempfile::tempdir().unwrap();
            let (mut h, net, conv) = a_chat_with_a_draft(&dir);
            state_cannot_send(&mut h, Some(reason.clone()));
            let conn = h.connection_mut();
            let before = stored_rows(conn);
            assert_eq!(before.1, 0, "{reason:?}: the premise, an empty outbox");

            let text = enqueue_text(conn, &net, conv, Text::plain("typed too late"), None, 1_000);
            assert_eq!(
                text.err().and_then(|e| e.refusal()),
                Some(Refusal::CannotSend),
                "{reason:?}: a text"
            );
            let files = enqueue_files(
                conn,
                &net,
                conv,
                files_of(2),
                Some(Text::plain("a caption")),
                None,
                1_000,
            );
            match files {
                Err(FilesError::Other(e)) => {
                    assert_eq!(e.refusal(), Some(Refusal::CannotSend), "{reason:?}: files")
                }
                Err(other) => {
                    panic!("{reason:?}: files were refused for another reason: {other:?}")
                }
                Ok(step) => panic!("{reason:?}: files were stored: {step:?}"),
            }
            let card = enqueue_contact(conn, &net, conv, contact_card(), 1_000);
            assert_eq!(
                card.err().and_then(|e| e.refusal()),
                Some(Refusal::CannotSend),
                "{reason:?}: a card"
            );
            assert_eq!(stored_rows(conn), before, "{reason:?}: nothing was written");
            assert!(draft_stays(conn, conv), "{reason:?}: the draft stays");
        }
    }

    /// The gate refuses on what is STATED: a chat with no features at all (a new chat the
    /// backend has not described yet) sends, as `ConversationRow.can_send` and the old
    /// behaviour of every send have it; so does a chat that states features without a
    /// reason. And a chat that cannot send now sends again once the backend clears the reason.
    #[test]
    fn a_chat_without_a_stated_reason_sends_and_one_that_lost_it_sends_again() {
        // No features stored.
        let dir = tempfile::tempdir().unwrap();
        let (mut h, net, conv) = a_chat_with_a_draft(&dir);
        let conn = h.connection_mut();
        assert_eq!(
            id_of(conn, "SELECT features IS NULL FROM conversations"),
            1,
            "the premise: no features stored"
        );
        enqueue_text(conn, &net, conv, Text::plain("one"), None, 1_000).unwrap();
        match enqueue_files(conn, &net, conv, files_of(2), None, None, 1_100) {
            Ok(FilesStep::Done(..)) => {}
            other => panic!("not stored whole: {other:?}"),
        }
        enqueue_contact(conn, &net, conv, contact_card(), 1_200).unwrap();
        assert_eq!(
            id_of(conn, "SELECT count(*) FROM outbox"),
            4,
            "a text, two files, a card"
        );
        assert!(!draft_stays(conn, conv), "the text took the draft");

        // Features without a reason.
        let dir = tempfile::tempdir().unwrap();
        let (mut h, net, conv) = a_chat_with_a_draft(&dir);
        state_cannot_send(&mut h, None);
        enqueue_text(
            h.connection_mut(),
            &net,
            conv,
            Text::plain("two"),
            None,
            1_000,
        )
        .unwrap();

        // A reason, then none.
        let dir = tempfile::tempdir().unwrap();
        let (mut h, net, conv) = a_chat_with_a_draft(&dir);
        state_cannot_send(&mut h, Some(kourier_model::CannotSend::NotAccepted));
        let refused = enqueue_text(
            h.connection_mut(),
            &net,
            conv,
            Text::plain("too early"),
            None,
            1_000,
        );
        assert_eq!(
            refused.err().and_then(|e| e.refusal()),
            Some(Refusal::CannotSend)
        );
        assert!(draft_stays(h.connection(), conv));
        state_cannot_send(&mut h, None);
        let conn = h.connection_mut();
        enqueue_text(conn, &net, conv, Text::plain("accepted now"), None, 2_000).unwrap();
        match enqueue_files(conn, &net, conv, files_of(1), None, None, 2_100) {
            Ok(FilesStep::Done(..)) => {}
            other => panic!("not stored whole: {other:?}"),
        }
        assert_eq!(
            id_of(conn, "SELECT count(*) FROM outbox"),
            2,
            "only the sends after the reason went"
        );
        assert!(!draft_stays(conn, conv), "the text took the draft");
    }

    /// G4: only the FIRST part of a send is gated. A send of more than `ENQUEUE_CHUNK`
    /// messages whose chat loses the right to send after its first part is finished: every
    /// message is queued, in order, as for a chat that kept the right (a half of a send
    /// would otherwise stay `Staged` for ever, or be dropped by the caller after the user
    /// was told "sent"). The next NEW send into that chat is refused.
    #[test]
    fn a_large_send_whose_chat_loses_the_right_after_its_first_part_still_finishes() {
        use kourier_model::CannotSend;
        let dir = tempfile::tempdir().unwrap();
        let (mut h, net, conv) = a_chat_with_a_draft(&dir);
        state_cannot_send(&mut h, None);
        let n = 2 * ENQUEUE_CHUNK + 22;
        let messages = store_in_parts(&mut h, &net, conv, n, 4_000, |h| {
            state_cannot_send(h, Some(CannotSend::NotMember));
            assert_eq!(
                id_of(
                    h.connection(),
                    "SELECT json_extract(features, '$.cannot_send') = 'NotMember'
                     FROM conversations"
                ),
                1,
                "the premise: the chat lost the right between the parts"
            );
        });
        assert_eq!(messages.len(), n);
        let conn = h.connection_mut();
        assert!(
            row_states(conn, &messages).iter().all(|s| s == QUEUED),
            "every message of the send is due"
        );
        assert_eq!(
            id_of(conn, "SELECT count(*) FROM outbox WHERE state = 'Staged'"),
            0
        );
        let next = enqueue_text(conn, &net, conv, Text::plain("another"), None, 5_000);
        assert_eq!(
            next.err().and_then(|e| e.refusal()),
            Some(Refusal::CannotSend),
            "a new send is gated again"
        );
    }
}
