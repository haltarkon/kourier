//! The deadline of ringing call lines (`ingest::call`): a line that rings past its deadline,
//! with no other signal, becomes a missed one, so nothing rings forever.
//!
//! The deadline is durable (`messages.call_deadline`, set by the ingest that stored the
//! ringing line, on this computer's clock and never further away than
//! `kourier_model::MAX_RING_MS`). One task per core sleeps on the injected
//! [`Clock`](kourier_backend_api::Clock) until the nearest one and then settles every line
//! that is due, in one transaction, with exactly what an ingested signal would change: the
//! row, the counters and the invalidations. It looks at once at a start (a line that came due
//! while Kourier was not running is settled by the first look) and whenever it is woken: by a
//! write that stored a ringing line ([`Changes::ringing`]) and by `UiCommand::Resumed` (a
//! tokio timer does not run while the machine is suspended; the sleep is also cut into steps
//! of at most [`crate::expiry::MAX_SLEEP`]).
//!
//! Who is told ([`Watch`]): a missed call the clock made is announced at once only when the
//! task settled it on time (within ten seconds of its deadline, `SETTLED_LATE_MS` in `ingest::call`) AND the account
//! is connected and caught up. A missed call nobody reported is only derived: after a start,
//! a resume or a reconnect the phone's sync that says "answered" may still be on its way, and
//! at a start the desktop may not listen yet. Such a line is HELD: it is unread at once, and
//! it is announced with the catch-up summary of its account, and only if it is still a missed
//! call then. The supervisor takes the held lines of its account into the summary it sends
//! when the account is online; lines held while it is online already (a late sweep, the quiet
//! time after a resume during which the backend may yet find its connection dead) are
//! released by this task as a summary of their own.
//!
//! A line that arrives already past its deadline never shows as ringing: ingest settles it
//! before it stores it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use kourier_db::Priority;
use kourier_db::rusqlite::{Connection, TransactionBehavior};
use kourier_model::{AccountId, ConversationId, MsgIdScope};

use crate::error::{CoreError, Result};
use crate::facade::UiEvent;
use crate::ingest::{Apply, Changes, NotifyCandidate};
use crate::runtime::Shared;
use crate::store::GLOBAL_LANE;

/// How long the task waits after a failed look or sweep.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// How long after a resume no account counts as caught up (ms): the backend's connection
/// may be dead without it knowing yet, and what the phone synced during the suspend comes
/// with the catch-up of the reconnect.
pub(crate) const RESUME_QUIET_MS: i64 = 30_000;

/// Which accounts may be told of a missed call at once, and the missed calls that wait for a
/// catch-up summary (module docs).
#[derive(Default)]
pub(crate) struct Watch {
    /// The accounts that are connected and done with their catch-up (`ConnState::Online`).
    online: BTreeSet<AccountId>,
    /// After a resume, nobody counts as caught up before this instant (core clock, ms).
    quiet_until: i64,
    /// The held lines, per account.
    held: BTreeMap<AccountId, Vec<NotifyCandidate>>,
}

impl Watch {
    fn caught_up(&self, account: AccountId, now: i64) -> bool {
        now >= self.quiet_until && self.online.contains(&account)
    }
}

impl Shared {
    fn calls_watch(&self) -> std::sync::MutexGuard<'_, Watch> {
        self.calls_watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The supervisor of `account` saw its connection state: `online` is "connected and
    /// caught up". Returns the missed calls that waited for that (for its summary).
    pub(crate) fn calls_online(&self, account: AccountId, online: bool) -> Vec<NotifyCandidate> {
        let mut w = self.calls_watch();
        if online {
            w.online.insert(account);
            w.held.remove(&account).unwrap_or_default()
        } else {
            w.online.remove(&account);
            Vec::new()
        }
    }

    /// The machine resumed (or the deadline task found a ring run out long ago, which says
    /// the same): for [`RESUME_QUIET_MS`] no account counts as caught up.
    pub(crate) fn calls_resumed(&self) {
        let until = self.now().saturating_add(RESUME_QUIET_MS);
        let mut w = self.calls_watch();
        w.quiet_until = w.quiet_until.max(until);
    }

    /// Missed calls of `account` that are not announced alone wait for its summary.
    pub(crate) fn hold_calls(&self, account: AccountId, lines: Vec<NotifyCandidate>) {
        if lines.is_empty() {
            return;
        }
        self.calls_watch()
            .held
            .entry(account)
            .or_default()
            .extend(lines);
        // The task releases them when their account is online already.
        self.calls_wake.notify_one();
    }

    /// The conversations of those of `lines` that are still unread (one entry per line):
    /// a held missed call counts in a summary only if nothing said meanwhile that it was
    /// answered, declined, read or deleted.
    pub(crate) async fn still_missed(&self, lines: Vec<NotifyCandidate>) -> Vec<ConversationId> {
        if lines.is_empty() {
            return Vec::new();
        }
        let read = self
            .store
            .read(move |c| {
                let mut out = Vec::new();
                let mut seen = BTreeSet::new();
                for l in lines {
                    if !seen.insert(l.message) {
                        continue;
                    }
                    let unread: bool = c
                        .prepare_cached(
                            "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1
                               AND read_state = 'Unread' AND deleted IS NULL)",
                        )?
                        .query_row([l.message.0], |r| r.get(0))?;
                    if unread {
                        out.push(l.conv);
                    }
                }
                Ok(out)
            })
            .await;
        match read {
            Ok(convs) => convs,
            Err(CoreError::Stopped) => Vec::new(),
            Err(e) => {
                tracing::warn!(error = %e, "reading held call lines failed");
                Vec::new()
            }
        }
    }
}

/// The nearest deadline of a ringing line (`None`: nothing rings).
pub(crate) fn next_deadline(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(
            "SELECT min(call_deadline) FROM messages INDEXED BY messages_ringing
             WHERE call_deadline IS NOT NULL",
        )?
        .query_row([], |r| r.get(0))?)
}

/// Settles every ringing line that is due at `now`, in one transaction. `caught_up` says
/// whether an account is connected and done with its catch-up (`Apply::settle_calls`).
/// Returns what changed per account, with the notifications to show and the lines to hold.
pub(crate) fn sweep(
    conn: &mut Connection,
    now: i64,
    caught_up: &dyn Fn(AccountId) -> bool,
) -> Result<Vec<(AccountId, Changes)>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let accounts: Vec<i64> = tx
        .prepare_cached(
            "SELECT DISTINCT account_id FROM messages INDEXED BY messages_ringing
             WHERE call_deadline IS NOT NULL AND call_deadline <= ?1",
        )?
        .query_map([now], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for account in accounts {
        let account = AccountId(account);
        // The scope only matters for key lookups, which settling does not do.
        let mut apply = Apply::begin(&tx, account, MsgIdScope::AccountAuthor, now)?;
        apply.settle_calls(caught_up(account))?;
        apply.finish()?;
        out.push((account, std::mem::take(&mut apply.ch)));
    }
    tx.commit()?;
    Ok(out)
}

/// The task (spawned by `Core::start`, aborted by `Core::shutdown`).
pub(crate) async fn run(sh: Arc<Shared>) {
    loop {
        if sh.stopping() {
            return;
        }
        release(&sh).await;
        let next = match sh.store.read(next_deadline).await {
            Ok(next) => next,
            Err(CoreError::Stopped) => return,
            Err(e) => {
                tracing::warn!(error = %e, "reading the next call deadline failed");
                wait(&sh, Some(retry_at(&sh))).await;
                continue;
            }
        };
        match next {
            Some(at) if at <= sh.now() => {
                if !sweep_once(&sh).await {
                    wait(&sh, Some(retry_at(&sh))).await;
                }
            }
            later => {
                // Lines held through the quiet time after a resume are looked at when it ends.
                let quiet = {
                    let w = sh.calls_watch();
                    let waiting = w.held.keys().any(|a| w.online.contains(a));
                    (waiting && w.quiet_until > sh.now()).then_some(w.quiet_until)
                };
                let until = match (later, quiet) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                wait(&sh, until).await;
            }
        }
    }
}

/// Announces, as a summary of their own, the held lines of every account that is online and
/// caught up now (the supervisor took what waited when the account came online; these were
/// held after that).
async fn release(sh: &Arc<Shared>) {
    let ready: Vec<(AccountId, Vec<NotifyCandidate>)> = {
        let now = sh.now();
        let mut w = sh.calls_watch();
        let accounts: Vec<AccountId> = w
            .held
            .keys()
            .copied()
            .filter(|a| w.caught_up(*a, now))
            .collect();
        accounts
            .into_iter()
            .filter_map(|a| w.held.remove(&a).map(|lines| (a, lines)))
            .collect()
    };
    for (account, lines) in ready {
        let convs = sh.still_missed(lines).await;
        if convs.is_empty() {
            continue;
        }
        let distinct: BTreeSet<ConversationId> = convs.iter().copied().collect();
        sh.emit(UiEvent::CatchUpSummary {
            account,
            messages: u32::try_from(convs.len()).unwrap_or(u32::MAX),
            conversations: u32::try_from(distinct.len()).unwrap_or(u32::MAX),
        });
    }
}

fn retry_at(sh: &Shared) -> i64 {
    let wait = i64::try_from(RETRY_AFTER.as_millis()).unwrap_or(i64::MAX);
    sh.now().saturating_add(wait)
}

/// Waits until `deadline` on the core clock, or until something woke the task: a write that
/// stored a ringing line, a line that is held, or a resume.
async fn wait(sh: &Shared, deadline: Option<i64>) {
    match deadline {
        Some(at) => {
            let at = crate::expiry::sleep_until(sh.now(), at);
            tokio::select! {
                () = sh.clock.sleep_until(at) => {}
                () = sh.calls_wake.notified() => {}
            }
        }
        None => sh.calls_wake.notified().await,
    }
}

/// One sweep and its publication; `false` when it failed.
async fn sweep_once(sh: &Arc<Shared>) -> bool {
    let state = sh.clone();
    let swept = sh
        .store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            if state.stopping() {
                return Ok((Vec::new(), false));
            }
            // The clock is read where the write runs: it is the instant the rows are judged
            // at, and so is who is caught up.
            let now = state.now();
            let caught_up: BTreeSet<AccountId> = {
                let w = state.calls_watch();
                w.online
                    .iter()
                    .copied()
                    .filter(|a| w.caught_up(*a, now))
                    .collect()
            };
            let done = sweep(c, now, &|account| caught_up.contains(&account))?;
            // A line of an account that is online was held: its ring ran out long before
            // this look, so the task (the machine) was not awake when it did.
            let slept = done
                .iter()
                .any(|(a, ch)| caught_up.contains(a) && !ch.held_calls.is_empty());
            Ok((done, slept))
        })
        .await;
    match swept {
        Ok((done, slept)) => {
            if slept {
                sh.calls_resumed();
            }
            for (account, mut ch) in done {
                tracing::debug!(
                    account = account.0,
                    lines = ch.messages.values().map(|m| m.len()).sum::<usize>(),
                    held = ch.held_calls.len(),
                    "call lines past their deadline are missed"
                );
                let held = std::mem::take(&mut ch.held_calls);
                crate::runtime::emit_changes(sh, &ch);
                sh.notify(account, &ch.notify);
                sh.hold_calls(account, held);
            }
            true
        }
        Err(CoreError::Stopped) => true,
        Err(e) => {
            tracing::warn!(error = %e, "settling call lines failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use kourier_model::{MsgIdScope, NetworkKind, UserId};

    /// `core.db` has no planner statistics: what the deadline task reads at every look must
    /// go through the partial index of the ringing lines, never through the whole table (the
    /// statements of [`super::next_deadline`], [`super::sweep`] and `Apply::settle_calls`).
    #[test]
    fn the_deadline_reads_use_the_index_of_ringing_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([3u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        let statements = [
            "SELECT min(call_deadline) FROM messages INDEXED BY messages_ringing
             WHERE call_deadline IS NOT NULL",
            "SELECT DISTINCT account_id FROM messages INDEXED BY messages_ringing
             WHERE call_deadline IS NOT NULL AND call_deadline <= 1000",
            "SELECT id FROM messages INDEXED BY messages_ringing
             WHERE call_deadline IS NOT NULL AND call_deadline <= 1000 AND +account_id = 1
             ORDER BY call_deadline, id",
        ];
        for sql in statements {
            let steps: Vec<String> = h
                .connection()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("prepare")
                .query_map([], |r| r.get::<_, String>(3))
                .expect("plan")
                .collect::<kourier_db::rusqlite::Result<_>>()
                .expect("rows");
            let on_messages: Vec<&String> =
                steps.iter().filter(|s| s.contains("messages")).collect();
            assert!(
                !on_messages.is_empty()
                    && on_messages.iter().all(|s| s.contains("messages_ringing")),
                "{sql}\n{steps:#?}"
            );
        }
        // And what every signal of a call looks its line up with (`Apply::call_row`): the
        // key of the conversation, never a walk through the conversation's rows.
        let lookup = "SELECT id FROM messages WHERE conversation_id = 1 AND net_local = 'call:1'
                        AND net_scope = 1";
        let steps: Vec<String> = h
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {lookup}"))
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(3))
            .expect("plan")
            .collect::<kourier_db::rusqlite::Result<_>>()
            .expect("rows");
        assert!(
            steps.len() == 1 && steps[0].contains("messages_key_conv"),
            "{steps:#?}"
        );
    }
}
