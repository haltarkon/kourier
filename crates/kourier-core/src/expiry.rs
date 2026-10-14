//! Disappearing messages (§3.5 `Expiry`, §11 S13): the sweeper.
//!
//! A message's timer starts per its kind: `AfterSend` at the send (`sent_at`), `AfterRead` when
//! the message is read here or on another device (our own `AfterRead` messages at the send),
//! unless the backend says when it started. A message sent from this device starts its timer
//! when the first recipient has it (Signal Desktop), so one queued offline never goes unsent.
//! Starting it sets `messages.expires_at` (indexed by `messages_expiring`). One task per core
//! sleeps on the injected [`Clock`] until the earliest `expires_at`, then deletes every message
//! whose time has come (rows, revisions, reactions, attachments, outbox rows, and the files,
//! unlinked after the commit; the account's dispatcher is told when outbox rows went, so
//! what waited behind them starts at once) and sleeps again. Writes that start a timer wake it
//! ([`Changes::expiring`](crate::ingest::Changes)); after a restart it starts from the stored
//! `expires_at`, so a message that expired while Kourier was not running goes at the first look.
//!
//! The sweeper removes rows, so it also remembers the key of every message it deleted
//! (`expired_messages`) for [`REMEMBER`]: ingest drops a message with such a key, so a late
//! redelivery (an envelope the server sends again, a second copy, an echo of our own send, a
//! history import) cannot bring an expired message back.
//!
//! # A clock nobody confirmed (§9.6)
//!
//! "Its time has come" is read from this computer's clock, and that clock can be wrong: hours
//! or years ahead, every timer looks run out, and a delete cannot be taken back. A backend
//! knows better, because its server states the time when it takes the connection. So an
//! account's clock counts as *confirmed* only while its backend is, or last was, connected
//! (`Syncing` or `Online`), and as *unconfirmed*
//!
//! - from the moment its backend reports the clock wrong
//!   (`ConnState::NeedsUserAction(FixClock)`) until a server took its connection again. This
//!   is stored (`settings`), so it holds across a restart, before the backend said anything;
//! - from the start of the core until its backend gave its first verdict in this run, for an
//!   account that is linked at that start: a clock that went wrong while Kourier was not
//!   running has no stored trace, and a start without a network gets no verdict at all. An
//!   account without a running backend (logged out, halted, removed) waits for nobody.
//!
//! What happens to a message of an unconfirmed account whose time has come by the local clock
//! is the user's choice, [`ExpiryPolicy`] (one setting for the whole core, applied per
//! account, since each account has its own backend to confirm the clock):
//!
//! - [`HideUntilConfirmed`](ExpiryPolicy::HideUntilConfirmed), the default: the message is
//!   hidden at once (`messages.expiry_hidden`) and erased when the clock is confirmed. Hidden
//!   means gone from every read that shows message content (timeline, list preview, unread
//!   and mention counts, quotes, the versions of a message, attachments and their media
//!   handles and plaintext playback copies), and commands that name it are refused as for a
//!   message that is gone; the invalidations close its notification. Nothing of it is
//!   deleted: when the confirmed time says it is not due yet, it shows again as it was, with
//!   its read state and counters.
//! - [`KeepUntilConfirmed`](ExpiryPolicy::KeepUntilConfirmed): nothing is hidden and nothing
//!   is erased until the clock is confirmed.
//! - [`EraseByLocalClock`](ExpiryPolicy::EraseByLocalClock): the local clock decides, whatever
//!   a backend says, as Signal Desktop does. Nothing is ever held.
//!
//! The sweeper is the only writer of the hidden flag. Every look does three things, in one
//! transaction: erase what is due in the accounts that are not held, hide what is due in the
//! accounts that are (first option), and show again what is hidden but no longer should be
//! (not due after the clock was set right, or the option changed). A change of the option
//! runs the same step in the transaction that stores it (a change to the third option hides
//! there what the next look then erases), and a start runs it before anything is served, so
//! a query never sees a state the option does not allow for longer than the moment between a
//! timer running out and the sweeper's wake-up, as for a delete. The option the sweeper
//! follows is the stored one: it changes in memory behind the commit that stored it, on the
//! writer thread, where every sweep reads it.
//!
//! A wrong clock nobody reports later (the backend was connected before, or its network does
//! not check) is not noticed.
//!
//! [`Clock`]: kourier_backend_api::Clock

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kourier_db::Priority;
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use kourier_model::{
    AccountId, AttachmentId, ConnState, ConversationId, MessageId, MsgIdScope, UserAction,
};

use crate::error::{CoreError, Result};
use crate::ingest::{Apply, Changes};
use crate::runtime::Shared;
use crate::store::GLOBAL_LANE;

/// Most messages one sweep transaction deletes (the rest follow at once).
const SWEEP_BATCH: i64 = 200;

/// How long the sweeper waits after a failed sweep before it looks again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Longest single sleep. Deadlines are wall-clock times, but tokio timers run on the monotonic
/// clock, which stops while the machine is suspended (and ignores clock adjustments): a sleep
/// of hours would end hours late after a suspend. Waking at least this often bounds the delay.
pub(crate) const MAX_SLEEP: Duration = Duration::from_secs(60);

/// How long the key of an expired message is remembered, so that a late copy of it is dropped
/// instead of shown again. A server keeps an undelivered envelope for weeks at most (Signal: 45
/// days, the age at which Desktop gives up on a stored envelope too,
/// `SD/ts/sql/Server.node.ts`), so nothing older can still arrive.
pub const REMEMBER: Duration = Duration::from_secs(45 * 24 * 3600);

/// What happens to a disappearing message whose time has come by this computer's clock while
/// no backend has confirmed that clock: a start without a network, or a clock a backend
/// reported as wrong (§9.6). The user's choice (decision of 2026-10-07), one setting for the
/// whole core: [`UiCommand::SetExpiryPolicy`](crate::facade::UiCommand::SetExpiryPolicy)
/// changes it, [`Query::ExpiryPolicy`](crate::facade::Query::ExpiryPolicy) reads it. It is
/// stored in `core.db`, so it holds across restarts and before any UI is up, and a change
/// takes effect at once in both directions. Once a backend took the connection, a due message
/// is erased under every option.
///
/// None of the three is a limit of a reference app: the first two are Kourier's protection
/// against a wrong clock, the third is Signal Desktop's behaviour for those who want it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExpiryPolicy {
    /// "Hide now, erase once the time is confirmed" (the default). A message that is due by
    /// the local clock is gone at once from everything the user or the desktop can see:
    /// timeline, list preview and unread counts, notifications, quotes of it, media handles
    /// and playback copies. It is erased for good (rows, files, the remembered key) only when
    /// a backend confirmed the clock, and when the confirmed time says it is not due yet it
    /// shows again exactly as before.
    #[default]
    HideUntilConfirmed,
    /// "Keep until the time is confirmed": nothing is hidden or erased while the clock is
    /// unconfirmed. A message whose time has come stays readable until the first connect.
    KeepUntilConfirmed,
    /// "Erase by this computer's clock": due messages are erased at once, whatever a backend
    /// says, as Signal Desktop does. A clock that is ahead erases messages early, for good.
    EraseByLocalClock,
}

impl ExpiryPolicy {
    /// Every option, in the order a settings page lists them (the default first).
    pub const ALL: [Self; 3] = [
        Self::HideUntilConfirmed,
        Self::KeepUntilConfirmed,
        Self::EraseByLocalClock,
    ];

    /// The stable name of the option (`"hide"`, `"keep"`, `"erase"`): what `core.db` stores,
    /// and a key a UI can use for the choice. Never translated.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HideUntilConfirmed => "hide",
            Self::KeepUntilConfirmed => "keep",
            Self::EraseByLocalClock => "erase",
        }
    }

    /// The option with the stable name `name` ([`as_str`](Self::as_str)).
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

/// The `settings` row of the option: one for the whole core (`scope` is not an account's).
const POLICY_SCOPE: &str = "core";
const POLICY_KEY: &str = "expiry_policy";

/// The stored [`ExpiryPolicy`]; the default when none was ever chosen, and for a value this
/// build does not know (written by a newer one): the default erases nothing early.
pub(crate) fn stored_policy(conn: &Connection) -> Result<ExpiryPolicy> {
    let name: Option<String> = conn
        .prepare_cached("SELECT value FROM settings WHERE scope = ?1 AND key = ?2")?
        .query_row((POLICY_SCOPE, POLICY_KEY), |r| r.get(0))
        .optional()?;
    Ok(name
        .as_deref()
        .and_then(ExpiryPolicy::from_name)
        .unwrap_or_default())
}

fn store_policy(conn: &Connection, policy: ExpiryPolicy) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO settings(scope, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
    )?
    .execute((POLICY_SCOPE, POLICY_KEY, policy.as_str()))?;
    Ok(())
}

/// What the sweeper works from at one look: the option, and the accounts whose clock no
/// backend has confirmed ([`Shared::expiry_hold`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hold {
    /// The user's choice.
    pub policy: ExpiryPolicy,
    /// The accounts whose backend found the local clock wrong, or has not said anything
    /// about it since the core started.
    pub unconfirmed: Vec<AccountId>,
}

impl Hold {
    /// The accounts whose due messages are not erased now.
    fn kept(&self) -> &[AccountId] {
        match self.policy {
            ExpiryPolicy::EraseByLocalClock => &[],
            _ => &self.unconfirmed,
        }
    }

    /// The accounts whose due messages are hidden instead.
    fn hiding(&self) -> &[AccountId] {
        match self.policy {
            ExpiryPolicy::HideUntilConfirmed => &self.unconfirmed,
            _ => &[],
        }
    }

    /// The messages to erase at `?1`: due, in an account that is not held.
    fn erasable(&self) -> String {
        format!(
            "expires_at IS NOT NULL AND expires_at <= ?1{}",
            not_in("account_id", self.kept())
        )
    }

    /// The messages to hide at `?1`: due and still shown, in an account that hides. `None`
    /// when no account does.
    ///
    /// `+account_id`: the unary plus keeps SQLite from choosing an index by account. Without
    /// statistics its planner prefers the equality on `messages_local(account_id, …)` to
    /// the range on `messages_expiring`, and then reads every message of the account to
    /// find the few that are due: at each look of the sweeper and at every start, where
    /// every linked account is unconfirmed (12 ms instead of 0.01 ms on plain SQLite with
    /// 50,000 messages in memory; a cold, encrypted `core.db` pays more).
    fn hideable(&self) -> Option<String> {
        let ids = self.hiding();
        (!ids.is_empty()).then(|| {
            format!(
                "expires_at IS NOT NULL AND expires_at <= ?1 AND expiry_hidden = 0
                 AND +account_id IN ({})",
                id_list(ids)
            )
        })
    }

    /// The hidden messages to show again at `?1`: not due by the clock as it reads now (it
    /// was set back, or the timer was reset), or in an account that only keeps.
    fn showable(&self) -> String {
        let kept = match self.policy {
            ExpiryPolicy::KeepUntilConfirmed if !self.unconfirmed.is_empty() => {
                format!(" OR account_id IN ({})", id_list(&self.unconfirmed))
            }
            _ => String::new(),
        };
        format!("expiry_hidden = 1 AND (expires_at IS NULL OR expires_at > ?1{kept})")
    }

    /// The messages whose time, still to come at `?1`, the sweeper sleeps towards: every one
    /// it will erase or hide then.
    fn awaited(&self) -> String {
        let kept = match self.policy {
            ExpiryPolicy::KeepUntilConfirmed => not_in("account_id", &self.unconfirmed),
            _ => String::new(),
        };
        format!("expires_at IS NOT NULL AND expires_at > ?1{kept}")
    }
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// The instant the sweeper sleeps until when the next expiry is at `deadline`: never more than
/// [`MAX_SLEEP`] after `now` (and never before the epoch).
pub(crate) fn sleep_until(now: i64, deadline: i64) -> u64 {
    let cap = now.saturating_add(ms(MAX_SLEEP));
    u64::try_from(deadline.min(cap).max(0)).unwrap_or(0)
}

/// `1, 2, 3` (ids only, so the text is safe to splice into a statement).
fn id_list(ids: &[AccountId]) -> String {
    let ids: Vec<String> = ids.iter().map(|a| a.0.to_string()).collect();
    ids.join(", ")
}

/// ` AND <column> NOT IN (…)` for `ids`; empty when there are none.
fn not_in(column: &str, ids: &[AccountId]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    format!(" AND {column} NOT IN ({})", id_list(ids))
}

/// What one look of the sweeper found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Look {
    /// Something is to be erased, hidden or shown again right now.
    pub work: bool,
    /// Else: when to look again, the earliest time at which something will be (`None`:
    /// nothing has a timer and nothing is hidden, so only a write can change that).
    pub next: Option<i64>,
}

/// Whether [`sweep`] has anything to do at `now` under `hold`, and if not, when it will.
pub(crate) fn look(conn: &Connection, now: i64, hold: &Hold) -> Result<Look> {
    let any = |rows: &str| -> Result<bool> {
        let sql = format!("SELECT EXISTS(SELECT 1 FROM messages WHERE {rows})");
        Ok(conn.prepare(&sql)?.query_row([now], |r| r.get(0))?)
    };
    let hide = match hold.hideable() {
        Some(rows) => any(&rows)?,
        None => false,
    };
    if hide || any(&hold.erasable())? || any(&hold.showable())? {
        return Ok(Look {
            work: true,
            next: None,
        });
    }
    let sql = format!(
        "SELECT min(expires_at) FROM messages WHERE {}",
        hold.awaited()
    );
    let next: Option<i64> = conn.prepare(&sql)?.query_row([now], |r| r.get(0))?;
    // No timer says when a clock is set back, and that is when a hidden message shows
    // again: while anything is hidden the sweeper keeps looking (every [`MAX_SLEEP`]).
    let hidden: bool = conn
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM messages WHERE expiry_hidden = 1)")?
        .query_row([], |r| r.get(0))?;
    Ok(Look {
        work: false,
        next: next.or(hidden.then_some(i64::MAX)),
    })
}

/// Whether a message with this key expired here within [`REMEMBER`] before `now` (§3.1: the
/// key is account-wide or per conversation, by the network's scope).
pub(crate) fn remembered(
    conn: &Connection,
    account: AccountId,
    scope: MsgIdScope,
    conv_key: &str,
    author: &str,
    local: &str,
    now: i64,
) -> Result<bool> {
    let since = now.saturating_sub(ms(REMEMBER));
    Ok(match scope {
        MsgIdScope::AccountAuthor => conn
            .prepare_cached(REMEMBERED_BY_AUTHOR)?
            .query_row((account.0, local, author, since), |r| r.get(0))?,
        MsgIdScope::Conversation => conn
            .prepare_cached(REMEMBERED_IN_CONV)?
            .query_row((account.0, local, conv_key, since), |r| r.get(0))?,
    })
}

// Run for every new message: each must start from the local id (`expired_messages_by_key` or
// a key of migration 14 that ends in it), never from all of an author's or a conversation's
// expired keys (`tests::a_new_message_looks_up_its_own_expired_key`).
const REMEMBERED_BY_AUTHOR: &str = "SELECT EXISTS(SELECT 1 FROM expired_messages
 WHERE account_id = ?1 AND net_local = ?2 AND net_scope = 0 AND net_author = ?3
   AND expired_at >= ?4)";
const REMEMBERED_IN_CONV: &str = "SELECT EXISTS(SELECT 1 FROM expired_messages
 WHERE account_id = ?1 AND net_local = ?2 AND net_scope = 1 AND conv_key = ?3
   AND expired_at >= ?4)";

/// What a sweep changed, per account.
type Swept = BTreeMap<AccountId, Changes>;

/// Sets `expiry_hidden` to `hidden` for the messages `rows` selects at `now`, recounts their
/// conversations and notes what the UI must read again. Hiding also drops the media handles
/// and the plaintext playback copies of their attachments.
fn flip(conn: &Connection, now: i64, rows: &str, hidden: bool, out: &mut Swept) -> Result<()> {
    let convs: Vec<(i64, i64)> = conn
        .prepare(&format!(
            "SELECT DISTINCT account_id, conversation_id FROM messages WHERE {rows}"
        ))?
        .query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    if convs.is_empty() {
        return Ok(());
    }
    if hidden {
        let media: Vec<(i64, i64)> = conn
            .prepare(&format!(
                "SELECT (SELECT account_id FROM messages WHERE id = a.message_id), a.id
                 FROM attachments a
                 WHERE a.message_id IN (SELECT id FROM messages WHERE {rows})"
            ))?
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (account, attachment) in media {
            let ch = out.entry(AccountId(account)).or_default();
            ch.media.insert(AttachmentId(attachment));
            ch.media_gone.insert(AttachmentId(attachment));
        }
    }
    conn.prepare(&format!(
        "UPDATE messages SET expiry_hidden = {} WHERE {rows}",
        i64::from(hidden)
    ))?
    .execute([now])?;
    for (account, conv) in convs {
        let conv = ConversationId(conv);
        crate::ingest::recount(conn, conv)?;
        let ch = out.entry(AccountId(account)).or_default();
        ch.conversations.insert(conv);
        ch.timelines.insert(conv);
    }
    Ok(())
}

/// Brings `expiry_hidden` in line with `hold` at `now`, inside the caller's transaction:
/// shows again what is hidden and should not be, hides what is due in the accounts that hide.
fn settle(conn: &Connection, now: i64, hold: &Hold, out: &mut Swept) -> Result<()> {
    flip(conn, now, &hold.showable(), false, out)?;
    if let Some(rows) = hold.hideable() {
        flip(conn, now, &rows, true, out)?;
    }
    Ok(())
}

/// At a start, before anything is served: `expiry_hidden` as `hold` wants it at `now`, so
/// what came due while Kourier was not running is hidden from the first query on (and what
/// a kill left hidden under an option that hides nothing shows again). Nobody listens yet,
/// so nothing is reported.
pub(crate) fn settle_at_start(conn: &mut Connection, now: i64, hold: &Hold) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    settle(&tx, now, hold, &mut Swept::new())?;
    tx.commit()?;
    Ok(())
}

/// `UiCommand::SetExpiryPolicy`: stores `policy` and, in the same transaction, hides or shows
/// what it says about the messages that are due now in `unconfirmed` accounts. What it
/// erases is the sweeper's next look (the caller wakes it): the rows and their files go in
/// the sweeper's batches, not in this command. Until then, a moment later, what "erase by
/// this computer's clock" is about to erase is hidden here, so that no read behind the
/// command shows a message the new option does not allow (a message kept and readable under
/// "keep" would otherwise show until the sweeper had its turn). Returns what changed per
/// account.
pub(crate) fn set_policy(
    conn: &mut Connection,
    policy: ExpiryPolicy,
    now: i64,
    unconfirmed: Vec<AccountId>,
) -> Result<Vec<(AccountId, Changes)>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    store_policy(&tx, policy)?;
    let mut out = Swept::new();
    let hold = Hold {
        policy,
        unconfirmed,
    };
    settle(&tx, now, &hold, &mut out)?;
    if policy == ExpiryPolicy::EraseByLocalClock {
        flip(&tx, now, DUE_AND_SHOWN, true, &mut out)?;
    }
    tx.commit()?;
    Ok(out.into_iter().collect())
}

/// The messages that are due at `?1` and still shown, in any account: under "erase by this
/// computer's clock" the sweeper's next look erases every one of them.
const DUE_AND_SHOWN: &str = "expires_at IS NOT NULL AND expires_at <= ?1 AND expiry_hidden = 0";

/// One look of the sweeper at `now`, in one transaction: hides and shows what `hold` says
/// ([`settle`]), deletes up to [`SWEEP_BATCH`] messages whose timer ran out in the accounts
/// that are not held and remembers their keys; forgets keys older than [`REMEMBER`]. Returns
/// what changed per account (files to unlink, timelines to reload) and whether more expired
/// messages are left.
pub(crate) fn sweep(
    conn: &mut Connection,
    now: i64,
    hold: &Hold,
) -> Result<(Vec<(AccountId, Changes)>, bool)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.prepare_cached("DELETE FROM expired_messages WHERE expired_at < ?1")?
        .execute([now.saturating_sub(ms(REMEMBER))])?;
    let mut out = Swept::new();
    settle(&tx, now, hold, &mut out)?;
    let sql = format!(
        "SELECT id, account_id, conversation_id FROM messages
         WHERE {} ORDER BY expires_at, id LIMIT ?2",
        hold.erasable()
    );
    let due: Vec<(i64, i64, i64)> = tx
        .prepare(&sql)?
        .query_map((now, SWEEP_BATCH + 1), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let more = i64::try_from(due.len()).unwrap_or(i64::MAX) > SWEEP_BATCH;
    let mut by_account: BTreeMap<AccountId, Vec<(MessageId, ConversationId)>> = BTreeMap::new();
    for (id, account, conv) in due
        .into_iter()
        .take(usize::try_from(SWEEP_BATCH).unwrap_or(0))
    {
        by_account
            .entry(AccountId(account))
            .or_default()
            .push((MessageId(id), ConversationId(conv)));
    }
    for (account, messages) in by_account {
        // The scope only matters for key lookups, which deleting does not do.
        let mut apply = Apply::begin(&tx, account, MsgIdScope::AccountAuthor, now)?;
        for (id, conv) in messages {
            apply.delete_attachments(id)?;
            tx.prepare_cached(
                "INSERT INTO expired_messages(account_id, net_scope, conv_key, net_author,
                   net_local, expired_at)
                 SELECT m.account_id, m.net_scope, c.conv_key, m.net_author, m.net_local, ?2
                 FROM messages m JOIN conversations c ON c.id = m.conversation_id
                 WHERE m.id = ?1",
            )?
            .execute((id.0, now))?;
            // Its outbox rows go with it (a send that waited for a retry, a queued edit or
            // reaction): what waited behind them in the conversation is the head now.
            let queued: bool = tx
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM outbox WHERE message_id = ?1)")?
                .query_row([id.0], |r| r.get(0))?;
            apply.ch.outbox |= queued;
            tx.prepare_cached("DELETE FROM messages WHERE id = ?1")?
                .execute([id.0])?;
            apply.recount(conv)?;
            apply.ch.conversations.insert(conv);
            apply.ch.timelines.insert(conv);
        }
        let erased = std::mem::take(&mut apply.ch);
        out.entry(account).or_default().absorb(erased);
    }
    tx.commit()?;
    Ok((out.into_iter().collect(), more))
}

/// The sweeper task (spawned by `Core::start`, aborted by `Core::shutdown`).
pub(crate) async fn run(sh: Arc<Shared>) {
    loop {
        if sh.stopping() {
            return;
        }
        let (hold, now) = (sh.expiry_hold(), sh.now());
        let found = match sh.store.read(move |c| look(c, now, &hold)).await {
            Ok(found) => found,
            Err(CoreError::Stopped) => return,
            Err(e) => {
                tracing::warn!(error = %e, "reading the next message expiry failed");
                wait(&sh, Some(retry_at(&sh))).await;
                continue;
            }
        };
        if !found.work {
            wait(&sh, found.next).await;
        } else if !sweep_once(&sh).await {
            wait(&sh, Some(retry_at(&sh))).await;
        }
    }
}

/// Whether `state` says the local clock cannot be trusted.
pub(crate) fn clock_is_wrong(state: &ConnState) -> bool {
    matches!(
        state,
        ConnState::NeedsUserAction(UserAction::FixClock { .. })
    )
}

/// Whether `state` says a server took our connection, which it does not with a clock it
/// would complain about: the clock can be trusted again.
pub(crate) fn clock_is_accepted(state: &ConnState) -> bool {
    matches!(state, ConnState::Online { .. } | ConnState::Syncing { .. })
}

fn retry_at(sh: &Shared) -> i64 {
    let wait = i64::try_from(RETRY_AFTER.as_millis()).unwrap_or(i64::MAX);
    sh.now().saturating_add(wait)
}

/// Waits until `deadline` on the core clock, or until something woke the sweeper: a write
/// that started a timer, a verdict on a clock, a change of the option.
async fn wait(sh: &Shared, deadline: Option<i64>) {
    match deadline {
        Some(at) => {
            let at = sleep_until(sh.now(), at);
            tokio::select! {
                () = sh.clock.sleep_until(at) => {}
                () = sh.expiry_wake.notified() => {}
            }
        }
        None => sh.expiry_wake.notified().await,
    }
}

/// One sweep transaction and its publication; `false` when it failed.
async fn sweep_once(sh: &Arc<Shared>) -> bool {
    let state = sh.clone();
    let swept = sh
        .store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            // The core is going down: who is held was decided for the run that ends here,
            // and the supervisors on their way out no longer say (§9.6).
            if state.stopping() {
                return Ok((Vec::new(), false));
            }
            // Read where the write runs, not where it was asked for: a verdict on a clock
            // or a change of the option in between counts.
            sweep(c, state.now(), &state.expiry_hold())
        })
        .await;
    match swept {
        // Expired messages left beyond the batch are found by the next look at once.
        Ok((done, _more)) => {
            for (account, ch) in done {
                tracing::debug!(
                    account = account.0,
                    "disappearing messages swept (erased, hidden or shown again)"
                );
                if ch.outbox {
                    // The rows behind a deleted head start now, not at that row's timer.
                    sh.kick_outbox(account);
                }
                match sh.blob_dir_of(account).await {
                    Ok(dir) => sh.publish_local(account, &dir, ch).await,
                    Err(e) => {
                        tracing::warn!(account = account.0, error = %e, "no blob directory for swept files")
                    }
                }
            }
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "sweeping disappearing messages failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lookup of [`remembered`], run for every new message, uses the message's local id:
    /// an index of the author or the conversation alone walks all of their expired keys (45
    /// days of a busy chat with disappearing messages: 40 ms a message at 45,000 keys).
    #[test]
    fn a_new_message_looks_up_its_own_expired_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([3u8; 32]),
            kourier_model::NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[kourier_model::UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        for sql in [REMEMBERED_BY_AUTHOR, REMEMBERED_IN_CONV] {
            let steps: Vec<String> = h
                .connection()
                .prepare(&format!(
                    "EXPLAIN QUERY PLAN {}",
                    sql.replace("?1", "1")
                        .replace("?2", "'1700000000000'")
                        .replace("?3", "'aci:carol'")
                        .replace("?4", "0")
                ))
                .expect("prepare")
                .query_map([], |r| r.get::<_, String>(3))
                .expect("plan")
                .collect::<kourier_db::rusqlite::Result<_>>()
                .expect("rows");
            let search: Vec<&String> = steps
                .iter()
                .filter(|s| s.contains("expired_messages"))
                .collect();
            assert!(
                !search.is_empty() && search.iter().all(|s| s.contains("net_local=?")),
                "{sql}\n{steps:#?}"
            );
        }
    }

    /// A deadline far away is slept towards in steps of at most [`MAX_SLEEP`]: a tokio timer
    /// does not run while the machine is suspended, so one long sleep would end late by the
    /// whole suspend.
    #[test]
    fn a_sleep_is_capped() {
        let now = 1_700_000_000_000;
        let minute = ms(MAX_SLEEP);
        assert_eq!(sleep_until(now, now + 5_000), (now + 5_000) as u64);
        assert_eq!(sleep_until(now, now + minute), (now + minute) as u64);
        assert_eq!(
            sleep_until(now, now + 7 * 24 * 3600 * 1000),
            (now + minute) as u64,
            "a week away: look again in a minute"
        );
        assert_eq!(sleep_until(now, i64::MAX), (now + minute) as u64);
        assert_eq!(sleep_until(now, now - 1), (now - 1) as u64, "due: at once");
        assert_eq!(sleep_until(-5, -1), 0, "never before the epoch");
        assert_eq!(sleep_until(i64::MAX, i64::MAX), i64::MAX as u64);
    }

    #[test]
    fn held_accounts_are_spliced_as_ids() {
        assert_eq!(not_in("account_id", &[]), "");
        assert_eq!(
            not_in("account_id", &[AccountId(3), AccountId(11)]),
            " AND account_id NOT IN (3, 11)"
        );
        assert!(clock_is_wrong(&ConnState::NeedsUserAction(
            UserAction::FixClock { skew_ms: -9_000 }
        )));
        assert!(!clock_is_wrong(&ConnState::Online { detail: None }));
        assert!(!clock_is_wrong(&ConnState::Offline { retry_at_ms: None }));
        assert!(clock_is_accepted(&ConnState::Online { detail: None }));
        assert!(clock_is_accepted(&ConnState::Syncing { fraction: None }));
        // Not connected says nothing about the clock: a hold stays.
        assert!(!clock_is_accepted(&ConnState::Connecting));
        assert!(!clock_is_accepted(&ConnState::Offline {
            retry_at_ms: None
        }));
    }

    /// Every read of the sweeper finds its rows through the index of the timers
    /// (`messages_expiring`) or of the hidden rows (`messages_expiry_hidden`), under every
    /// option: none walks the messages of an account. At a start every linked account is
    /// unconfirmed, so a read that did would walk the whole table before anything is
    /// served, and again at every look of the sweeper.
    #[test]
    fn no_read_of_the_sweeper_walks_the_messages_of_an_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let own = kourier_model::UserId::parse("aci:00000000-0000-4000-8000-0000000000aa");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([7u8; 32]),
            kourier_model::NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[own.expect("user id")],
        )
        .expect("harness");
        let conn = h.connection();
        let plan = |sql: &str| -> Vec<String> {
            conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("prepare")
                .query_map([1_000_i64], |r| r.get::<_, String>(3))
                .expect("plan")
                .collect::<kourier_db::rusqlite::Result<_>>()
                .expect("rows")
        };
        for policy in ExpiryPolicy::ALL {
            for unconfirmed in [vec![], vec![AccountId(1)], vec![AccountId(1), AccountId(2)]] {
                let hold = Hold {
                    policy,
                    unconfirmed,
                };
                let mut reads = vec![hold.erasable(), hold.showable(), hold.awaited()];
                reads.extend(hold.hideable());
                reads.push(DUE_AND_SHOWN.to_owned());
                for rows in reads {
                    let statements = [
                        format!("SELECT EXISTS(SELECT 1 FROM messages WHERE {rows})"),
                        format!(
                            "SELECT DISTINCT account_id, conversation_id FROM messages WHERE {rows}"
                        ),
                        format!("SELECT min(expires_at) FROM messages WHERE {rows}"),
                        format!("UPDATE messages SET expiry_hidden = 1 WHERE {rows}"),
                    ];
                    for sql in statements {
                        let steps = plan(&sql);
                        let on_messages: Vec<&String> =
                            steps.iter().filter(|s| s.contains("messages")).collect();
                        assert!(
                            !on_messages.is_empty()
                                && on_messages.iter().all(|s| {
                                    s.contains("messages_expiring")
                                        || s.contains("messages_expiry_hidden")
                                }),
                            "{hold:?}: {sql}\n{steps:#?}"
                        );
                    }
                }
            }
        }
    }

    /// The names are what `core.db` stores and what a UI keys its choice by: they never
    /// change, the default is the first option, and an unknown name is no option.
    #[test]
    fn the_options_have_stable_names_and_hiding_is_the_default() {
        assert_eq!(ExpiryPolicy::default(), ExpiryPolicy::HideUntilConfirmed);
        assert_eq!(ExpiryPolicy::ALL[0], ExpiryPolicy::default());
        let names: Vec<&str> = ExpiryPolicy::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(names, ["hide", "keep", "erase"]);
        for p in ExpiryPolicy::ALL {
            assert_eq!(ExpiryPolicy::from_name(p.as_str()), Some(p));
        }
        assert_eq!(ExpiryPolicy::from_name("Hide"), None);
        assert_eq!(ExpiryPolicy::from_name(""), None);
    }

    /// Which accounts each option holds and hides: the third never holds, only the first
    /// hides, and no row is selected by two of the three steps of a sweep (else it would be
    /// hidden and shown again for ever).
    #[test]
    fn each_option_selects_its_own_rows() {
        let unconfirmed = vec![AccountId(2), AccountId(5)];
        let hold = |policy| Hold {
            policy,
            unconfirmed: unconfirmed.clone(),
        };
        let hide = hold(ExpiryPolicy::HideUntilConfirmed);
        assert_eq!(hide.kept(), unconfirmed.as_slice());
        assert_eq!(hide.hiding(), unconfirmed.as_slice());
        assert!(hide.erasable().ends_with("AND account_id NOT IN (2, 5)"));
        assert!(
            hide.hideable()
                .expect("hides")
                .ends_with("account_id IN (2, 5)")
        );
        assert!(!hide.showable().contains("account_id"));
        assert!(!hide.awaited().contains("account_id"), "hides when due");

        let keep = hold(ExpiryPolicy::KeepUntilConfirmed);
        assert_eq!(keep.kept(), unconfirmed.as_slice());
        assert!(keep.hiding().is_empty());
        assert_eq!(keep.hideable(), None);
        assert!(keep.showable().contains("OR account_id IN (2, 5)"));
        assert!(keep.awaited().ends_with("AND account_id NOT IN (2, 5)"));

        let erase = hold(ExpiryPolicy::EraseByLocalClock);
        assert!(erase.kept().is_empty(), "never holds");
        assert!(erase.hiding().is_empty());
        assert!(!erase.erasable().contains("account_id"));
        assert_eq!(erase.hideable(), None);
        assert!(!erase.showable().contains("account_id"));

        // With every clock confirmed the three options are one.
        for policy in ExpiryPolicy::ALL {
            let none = Hold {
                policy,
                unconfirmed: Vec::new(),
            };
            assert!(!none.erasable().contains("account_id"));
            assert_eq!(none.hideable(), None);
            assert!(!none.showable().contains("account_id"));
        }
    }
}
