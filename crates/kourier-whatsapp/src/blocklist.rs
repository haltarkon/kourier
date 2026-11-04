//! The phone's blocklist, read once per start and at most once per 24 hours (W6a2, chunks B3 and B7z; decision D22 of
//! `target/wa/w6-decisions.md`).
//!
//! WHAT: the people the phone has blocked are told to the core as `ContactPatch.blocked`, so the person's page can say so.
//! There is no block action in W6a2 (nothing is written to WhatsApp), and the blocklist is not a source of merges
//! (D7: `blocklist_active` rows of the store's pairs are upstream's, and none exists at the pin).
//!
//! WHEN: ONE `blocklist` IQ per start, after the connection came `Online` ([`Reader::start`], called by the actor at that
//! moment and never before), by ONE task on the actor's runtime, and AT MOST ONCE PER 24 HOURS per account (the lead's
//! call after part 1, Q1): upstream asks the blocklist itself at every connection and drops the answer, so Kourier's read
//! is a second identical IQ, which the phone's own clients do not send at every start, and the list changes rarely. The
//! stamp (`blocklist_read`, migration 8) is written by the zone that states an ANSWER ([`state`]), so a read with no answer
//! stamps nothing; a link and a `Rebuild` delete it, so the first `Online` after either asks. A stamp younger than
//! [`REREAD_MS`] makes the task end at once with [`Outcome::Fresh`] (nothing asked, a reconnect looks again, so a start
//! that outlives the 24 hours reads once more). A read that never reached the server or got no answer in time
//! (`AskFailure::{NoClient, NotConnected, Timeout}`) is asked again at the NEXT `Online` of the start, and only then; a
//! server that answered with an error or an unreadable list is not asked again in this start (no retry loop; the next
//! start is the retry, and nothing was stamped).
//!
//! HOW: the answer's JIDs become people ([`people`]: a number or a LID, our own ids and everything that is no person
//! left out, each once), and ONE zone of the event writer ([`state`], `EventWriter::write_blocklist_and_wait`) compares
//! them with `blocked_stated` (migration 8, what the core was told): a person newly on the list is stated blocked, one
//! that is no longer on it is stated unblocked, in one `core_outbox` row, the unblocks first. Nothing is appended when
//! nothing changed. The same person under both of his ids is two statements; the core's alias makes them one contact.
//!
//! Counts only in the log: no JID, no number, no LID, no name.

use std::collections::BTreeSet;
use std::sync::Arc;

use kourier_backend_api::{BackendEvent, Clock};
use kourier_db::rusqlite::Transaction;
use kourier_model::{ContactPatch, UserId};
use tokio::task::JoinHandle;
use wacore::store::error::StoreError;
use wacore_binary::Jid;

use crate::events::EventWriter;
use crate::map::{self, OwnIds};
use crate::session::{AskFailure, Session};
use crate::store::{KourierStore, people};

/// The blocklist is asked at most once per this long per account (24 hours, in ms).
pub(crate) const REREAD_MS: i64 = 24 * 60 * 60 * 1000;

/// Whether a read is due: nothing was ever stated, the stamp is `REREAD_MS` old, or it lies in the FUTURE (a clock set
/// back must not silence the list for days).
pub(crate) fn due(stamp: Option<i64>, now_ms: i64) -> bool {
    stamp.is_none_or(|at| now_ms < at || now_ms - at >= REREAD_MS)
}

/// The people a blocklist names: a number or a LID each (`map::person_of`; a hosted device is the ordinary person), once
/// each, in id order. Our own ids are left out (the phone cannot block the account, and a statement about it is a lie),
/// and so is every entry that is no person.
pub(crate) fn people(listed: &[Jid], own: &OwnIds) -> Vec<UserId> {
    listed
        .iter()
        .filter_map(map::person_of)
        .filter(|person| !own.is_own(person))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// What one statement of the blocklist did, in numbers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) blocked: usize,
    pub(crate) unblocked: usize,
}

/// What the zone says for `listed` (the people the phone's list names now): the events, and the books moved to match.
/// The unblocks come first, so a person who changed his id between two reads is never blocked and unblocked in the
/// wrong order. The answer is STAMPED with `now_ms` (also when it changed nothing): the next read is due in 24 hours.
pub(crate) fn state(
    tx: &Transaction<'_>,
    listed: &[UserId],
    now_ms: i64,
) -> Result<(Vec<BackendEvent>, Counts), StoreError> {
    people::stamp_blocklist(tx, now_ms)?;
    let now: BTreeSet<&UserId> = listed.iter().collect();
    let mut events = Vec::new();
    let mut counts = Counts::default();
    for stated in people::blocked(tx)? {
        match UserId::parse(&stated) {
            // Still on the list: the core was told, nothing to say.
            Ok(person) if now.contains(&person) => {}
            Ok(person) => {
                people::drop_blocked(tx, &stated)?;
                events.push(blocked_patch(person, false));
                counts.unblocked += 1;
            }
            // A row that does not read as an id is dropped with no statement.
            Err(_) => {
                people::drop_blocked(tx, &stated)?;
            }
        }
    }
    for person in listed {
        if people::put_blocked(tx, person.as_str())? {
            events.push(blocked_patch(person.clone(), true));
            counts.blocked += 1;
        }
    }
    Ok((events, counts))
}

fn blocked_patch(person: UserId, blocked: bool) -> BackendEvent {
    let mut patch = ContactPatch::new(person);
    patch.blocked = Some(blocked);
    BackendEvent::ContactUpsert(patch)
}

/// How one run of the read ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The list was read and stated (it may have changed nothing).
    Stated,
    /// The last answer was stated less than 24 hours ago: nothing was asked (Q1). A reconnect looks again.
    Fresh,
    /// The server answered and the answer is no list (an error status, an unreadable list): not asked again in this
    /// start.
    Refused,
    /// Nothing was asked or nothing came back (no client, a dropped connection, no answer in time, a failed zone): the
    /// next `Online` of this start asks again.
    Later,
}

/// The task: asks the list once and states it, unless the last answer is younger than 24 hours. The log says what became
/// of it by kind and count.
pub(crate) async fn read(
    session: &dyn Session,
    store: &KourierStore,
    clock: &Arc<dyn Clock>,
    writer: &EventWriter,
    own: OwnIds,
) -> Outcome {
    let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
    match store.read(people::blocklist_read_at).await {
        Ok(stamp) if !due(stamp, now) => {
            tracing::debug!("the blocklist was stated less than a day ago; it is not asked");
            return Outcome::Fresh;
        }
        Ok(_) => {}
        Err(_) => {
            tracing::warn!(
                "the blocklist's stamp could not be read; it is asked again at the next connection"
            );
            return Outcome::Later;
        }
    }
    let listed = match session.blocklist().await {
        Ok(listed) => listed,
        Err(failure) => return failed(failure),
    };
    let named = people(&listed, &own);
    let (entries, writer) = (listed.len(), writer.clone());
    let count = named.len();
    let written =
        tokio::task::spawn_blocking(move || writer.write_blocklist_and_wait(&named)).await;
    match written {
        Ok(Ok(())) => {
            tracing::info!(entries, people = count, "the blocklist was read");
            Outcome::Stated
        }
        _ => {
            tracing::warn!(
                "the blocklist could not be stated; it is asked again at the next connection"
            );
            Outcome::Later
        }
    }
}

/// What an ask that got no list means for the start, with a log line of its kind.
fn failed(failure: AskFailure) -> Outcome {
    match failure {
        AskFailure::NoClient | AskFailure::NotConnected | AskFailure::Timeout => {
            tracing::warn!(
                kind = failure.kind(),
                "the blocklist could not be asked; it is asked again at the next connection"
            );
            Outcome::Later
        }
        AskFailure::Refused { code, .. } => {
            tracing::warn!(
                code,
                "the server refused the blocklist; it is not asked again in this start"
            );
            Outcome::Refused
        }
        AskFailure::Unreadable | AskFailure::Internal => {
            tracing::warn!(
                kind = failure.kind(),
                "the blocklist could not be read; it is not asked again in this start"
            );
            Outcome::Refused
        }
    }
}

/// The actor's side of the read: whether this start has it, and the one task that makes it.
#[derive(Default)]
pub(crate) struct Reader {
    /// The list was stated, or the server answered with no list: nothing more is asked in this start. A `Fresh` end
    /// leaves it false (the next `Online` looks at the stamp again).
    done: bool,
    task: Option<JoinHandle<Outcome>>,
}

impl Reader {
    /// The connection is `Online`: starts the task unless this start has the list or a task still runs. `spawn` runs the
    /// future the caller built over [`read`] (the actor knows the session, the writer and our ids).
    pub(crate) async fn start<F>(&mut self, spawn: impl FnOnce() -> F)
    where
        F: std::future::Future<Output = Outcome> + Send + 'static,
    {
        if let Some(task) = &self.task {
            if !task.is_finished() {
                return;
            }
            if let Some(task) = self.task.take() {
                // A task that died or came back `Later` leaves the list unread: this `Online` asks again.
                if matches!(task.await, Ok(Outcome::Stated | Outcome::Refused)) {
                    self.done = true;
                }
            }
        }
        if self.done {
            return;
        }
        self.task = Some(tokio::spawn(spawn()));
    }

    /// The client goes: a running task ends at once (it holds the client's session). The list stays unread if it was. A task
    /// that had FINISHED keeps its outcome (fix F7a, PROTOCOL-4): a refused list is not asked again in this start because
    /// the client was rebuilt (a handler's failed write ends the client, and the next one's `Online` would otherwise ask the
    /// refused list again, once per rebuild). Aborting a finished task changes nothing, so the join tells which it was.
    pub(crate) async fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            if matches!(task.await, Ok(Outcome::Stated | Outcome::Refused)) {
                self.done = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;
