//! The deadline of the waits for the other device's answer (`ingest::restore`): a request
//! for the files of a message that went out and got no (final) answer in time ends as "did
//! not answer", so nothing says "Asking your phone…" forever.
//!
//! The deadline is durable (`attachment_requests.deadline`, on this computer's clock: 30 s
//! after the backend reported the request sent, 2 min after each answer that says "still
//! uploading"). One task per core sleeps on the injected
//! [`Clock`](kourier_backend_api::Clock) until the nearest one and then ends every wait that
//! is due, in one transaction. It looks at once at a start (a wait whose deadline passed
//! while Kourier was closed ends at the first look) and whenever it is woken: by a write
//! that armed or ended a wait ([`Changes::restoring`]) and by `UiCommand::Resumed`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use kourier_db::Priority;
use kourier_db::rusqlite::{Connection, TransactionBehavior};
use kourier_model::{AccountId, AskFailure, MessageId, MsgIdScope};

use crate::error::{CoreError, Result};
use crate::ingest::{Apply, Changes};
use crate::runtime::Shared;
use crate::store::GLOBAL_LANE;

/// How long the task waits after a failed look before it looks again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// How long after an account's actor started its waits are not judged (ms) unless the account
/// is online already: the backend replays what it kept in its `core_outbox` from its own
/// database at the start, no network needed, and an answer that came in time must be applied
/// before the deadline that passed while Kourier was closed ends the wait ("did not answer"),
/// or the replayed answer would find no request and the file would only get a pointer.
pub(crate) const START_GRACE_MS: i64 = 10_000;

/// Which accounts' due waits may be judged ([`START_GRACE_MS`]).
#[derive(Default)]
pub(crate) struct Gate {
    /// When the actor of each account was started (core clock, ms).
    started: BTreeMap<AccountId, i64>,
    /// The accounts that are connected and caught up (`ConnState::Online`).
    online: BTreeSet<AccountId>,
}

impl Gate {
    /// Whether the waits of `account` that are due at `now` are judged. An account without
    /// an actor (logged out, idle) has nothing to replay.
    pub(crate) fn judged(&self, account: AccountId, now: i64) -> bool {
        match self.started.get(&account) {
            None => true,
            Some(since) => {
                self.online.contains(&account) || now >= since.saturating_add(START_GRACE_MS)
            }
        }
    }
}

impl Shared {
    fn restore_gate(&self) -> std::sync::MutexGuard<'_, Gate> {
        self.restore_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The supervisor started the actor of `account`: its first waits are held for
    /// [`START_GRACE_MS`].
    pub(crate) fn restore_actor_started(&self, account: AccountId) {
        let now = self.now();
        let mut gate = self.restore_gate();
        gate.started.insert(account, now);
        gate.online.remove(&account);
    }

    /// The supervisor saw the connection state of `account`: `online` is "connected and
    /// caught up", which ends the hold.
    pub(crate) fn restore_online(&self, account: AccountId, online: bool) {
        let mut gate = self.restore_gate();
        if online {
            gate.online.insert(account);
        } else {
            gate.online.remove(&account);
        }
        drop(gate);
        if online {
            self.restore_wake.notify_one();
        }
    }

    /// Whether the due waits of `account` are judged at `now`.
    fn restore_judged(&self, account: AccountId, now: i64) -> bool {
        self.restore_gate().judged(account, now)
    }
}

/// The nearest deadline of a wait (`None`: nothing waits with a deadline).
pub(crate) fn next_deadline(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(
            "SELECT min(deadline) FROM attachment_requests INDEXED BY attachment_requests_deadline
             WHERE deadline IS NOT NULL",
        )?
        .query_row([], |r| r.get(0))?)
}

/// Ends every wait that is due at `now`, in one transaction: its request row goes and the
/// attachments still asked for become "did not answer". Returns what changed per account.
pub(crate) fn sweep(conn: &mut Connection, now: i64) -> Result<Vec<(AccountId, Changes)>> {
    Ok(sweep_where(conn, now, &|_| true)?.0)
}

/// [`sweep`] for the accounts `judged` accepts; the waits of the others stay, and the second
/// value says whether any did.
pub(crate) fn sweep_where(
    conn: &mut Connection,
    now: i64,
    judged: &dyn Fn(AccountId) -> bool,
) -> Result<(Vec<(AccountId, Changes)>, bool)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let due: Vec<(i64, i64)> = tx
        .prepare_cached(
            "SELECT account_id, message_id FROM attachment_requests
               INDEXED BY attachment_requests_deadline
             WHERE deadline IS NOT NULL AND deadline <= ?1 ORDER BY account_id",
        )?
        .query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut out: Vec<(AccountId, Changes)> = Vec::new();
    let mut held = false;
    let mut i = 0;
    while i < due.len() {
        let account = AccountId(due[i].0);
        if !judged(account) {
            held = true;
            while i < due.len() && due[i].0 == account.0 {
                i += 1;
            }
            continue;
        }
        // The scope only matters for key lookups, which ending a wait does not do.
        let mut apply = Apply::begin(&tx, account, MsgIdScope::AccountAuthor, now)?;
        while i < due.len() && due[i].0 == account.0 {
            apply.wait_ran_out(MessageId(due[i].1))?;
            i += 1;
        }
        apply.finish()?;
        out.push((account, std::mem::take(&mut apply.ch)));
    }
    tx.commit()?;
    Ok((out, held))
}

impl Apply<'_> {
    /// The wait for the answer about `message` ran out: "did not answer". A request whose
    /// message is gone is deleted too: left in place it would be due for ever and the task
    /// would sweep it again at once, in a loop (rows go with their message by `ON DELETE
    /// CASCADE`, but a migration that rebuilds a table with foreign keys off could leave one).
    fn wait_ran_out(&mut self, message: MessageId) -> Result<()> {
        match self.msg_row(message)? {
            Some(msg) => self.end_wait(&msg, AskFailure::NoAnswer),
            None => {
                self.conn
                    .prepare_cached("DELETE FROM attachment_requests WHERE message_id = ?1")?
                    .execute([message.0])?;
                self.ch.restoring = true;
                Ok(())
            }
        }
    }
}

/// Ends every open wait of `account` as "did not answer", inside the caller's transaction:
/// the account was seen logged out (or unlinked), so no request of it will ever be answered or
/// reported unsent. Returns what changed.
pub(crate) fn end_account_waits(
    conn: &Connection,
    account: AccountId,
    now: i64,
) -> Result<Changes> {
    let messages: Vec<i64> = conn
        .prepare_cached("SELECT message_id FROM attachment_requests WHERE account_id = ?1")?
        .query_map([account.0], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut apply = Apply::begin(conn, account, MsgIdScope::AccountAuthor, now)?;
    for message in messages {
        apply.wait_ran_out(MessageId(message))?;
    }
    apply.finish()?;
    Ok(std::mem::take(&mut apply.ch))
}

/// Every attachment that says it is being asked for although no request of its message is
/// open ends as "did not answer" (clickable again): such a state cannot end by itself. Run
/// once at a start; a message and its request change in one transaction, so only a backend
/// that delivers an attachment in that state (or an older build's data) leaves one.
pub(crate) fn end_orphan_asks(
    conn: &mut Connection,
    now: i64,
) -> Result<Vec<(AccountId, Changes)>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let orphans: Vec<(i64, i64)> = tx
        .prepare_cached(
            "SELECT DISTINCT m.account_id, a.message_id
             FROM attachments a INDEXED BY attachments_asking
               JOIN messages m ON m.id = a.message_id
             WHERE a.state = '\"Asking\"'
               AND NOT EXISTS (SELECT 1 FROM attachment_requests r WHERE r.message_id = a.message_id)
             ORDER BY m.account_id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut out: Vec<(AccountId, Changes)> = Vec::new();
    let mut i = 0;
    while i < orphans.len() {
        let account = AccountId(orphans[i].0);
        let mut apply = Apply::begin(&tx, account, MsgIdScope::AccountAuthor, now)?;
        while i < orphans.len() && orphans[i].0 == account.0 {
            if let Some(msg) = apply.msg_row(MessageId(orphans[i].1))? {
                apply.end_wait(&msg, AskFailure::NoAnswer)?;
            }
            i += 1;
        }
        apply.finish()?;
        out.push((account, std::mem::take(&mut apply.ch)));
    }
    tx.commit()?;
    Ok(out)
}

/// The task (spawned by `Core::start`, aborted by `Core::shutdown`).
pub(crate) async fn run(sh: Arc<Shared>) {
    end_orphans(&sh).await;
    loop {
        if sh.stopping() {
            return;
        }
        let next = match sh.store.read(next_deadline).await {
            Ok(next) => next,
            Err(CoreError::Stopped) => return,
            Err(e) => {
                tracing::warn!(error = %e, "reading the next restore deadline failed");
                wait(&sh, Some(retry_at(&sh))).await;
                continue;
            }
        };
        match next {
            Some(at) if at <= sh.now() => match sweep_once(&sh).await {
                Some(false) => {}
                // Waits of an account that has only just started wait for its replay: look
                // again soon (the end of the hold also wakes the task).
                Some(true) => wait(&sh, Some(sh.now().saturating_add(1_000))).await,
                None => wait(&sh, Some(retry_at(&sh))).await,
            },
            later => wait(&sh, later).await,
        }
    }
}

fn retry_at(sh: &Shared) -> i64 {
    let wait = i64::try_from(RETRY_AFTER.as_millis()).unwrap_or(i64::MAX);
    sh.now().saturating_add(wait)
}

/// Waits until `deadline` on the core clock, or until something woke the task.
async fn wait(sh: &Shared, deadline: Option<i64>) {
    match deadline {
        Some(at) => {
            let at = crate::expiry::sleep_until(sh.now(), at);
            tokio::select! {
                () = sh.clock.sleep_until(at) => {}
                () = sh.restore_wake.notified() => {}
            }
        }
        None => sh.restore_wake.notified().await,
    }
}

/// One sweep and its publication: whether waits were held back, `None` when it failed.
async fn sweep_once(sh: &Arc<Shared>) -> Option<bool> {
    let state = sh.clone();
    let swept = sh
        .store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            if state.stopping() {
                return Ok((Vec::new(), false));
            }
            // The clock is read where the write runs: it is the instant the rows are judged at.
            let now = state.now();
            sweep_where(c, now, &|account| state.restore_judged(account, now))
        })
        .await;
    match swept {
        Ok((done, held)) => {
            publish(sh, Ok(done), "ending waits for the other device failed").then_some(held)
        }
        Err(e) => publish(sh, Err(e), "ending waits for the other device failed").then_some(false),
    }
}

/// Once at a start: what says it is being asked for without a request ends
/// ([`end_orphan_asks`]).
async fn end_orphans(sh: &Arc<Shared>) {
    let state = sh.clone();
    let done = sh
        .store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            if state.stopping() {
                return Ok(Vec::new());
            }
            end_orphan_asks(c, state.now())
        })
        .await;
    publish(sh, done, "ending asks without a request failed");
}

/// Emits what a sweep changed; `false` when it failed (the task looks again later).
fn publish(
    sh: &Arc<Shared>,
    swept: Result<Vec<(AccountId, Changes)>>,
    failure: &'static str,
) -> bool {
    match swept {
        Ok(done) => {
            for (account, ch) in done {
                tracing::debug!(
                    account = account.0,
                    attachments = ch.media.len(),
                    "waits for the other device's answer ran out"
                );
                crate::runtime::emit_changes(sh, &ch);
            }
            true
        }
        Err(CoreError::Stopped) => true,
        Err(e) => {
            tracing::warn!(error = %e, "{}", failure);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use kourier_model::{MsgIdScope, NetworkKind, UserId};

    /// An account whose actor has just started is not judged until its backend replayed its
    /// outbox (the grace) or it is online; an account without an actor always is.
    #[test]
    fn the_first_waits_of_a_started_actor_are_held_until_its_replay_or_its_catch_up() {
        let mut gate = super::Gate::default();
        let account = kourier_model::AccountId(1);
        assert!(gate.judged(account, 5), "no actor: nothing to replay");
        gate.started.insert(account, 100);
        assert!(!gate.judged(account, 100));
        assert!(!gate.judged(account, 100 + super::START_GRACE_MS - 1));
        assert!(gate.judged(account, 100 + super::START_GRACE_MS));
        gate.online.insert(account);
        assert!(gate.judged(account, 101), "online: the replay is done");
        assert!(
            gate.judged(kourier_model::AccountId(2), 101),
            "another account"
        );
    }

    /// `core.db` has no planner statistics: what the task reads at every look goes through
    /// the partial index of the waits (`INDEXED BY` fails the statement otherwise).
    #[test]
    fn the_deadline_reads_use_the_index_of_waits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([3u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        assert_eq!(super::next_deadline(h.connection()).expect("next"), None);
        let swept = super::sweep(h.connection_mut(), 1000).expect("sweep");
        assert!(swept.is_empty());
    }
}
