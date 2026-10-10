//! Call lines: one row per call that later signals change (`kourier_model::call`).
//!
//! A backend reports every signal of a call as a `Message` whose body is a call line, all of
//! them under one id (`call:<id>`, `gcall:<id>`). The first makes the row; every later one is
//! merged into it by [`CallLine::join`], so any arrival order and any repetition end in the
//! same row. What does not follow the arrival order besides the line: `sent_at`, `server_at`,
//! `received_at` and `arrival` are the earliest any signal carried, so the line shows when the
//! call began, and the row is silent only when every signal was. Its place in the timeline
//! (`sort_key`) is the first arrival's, as for every live row (§3.9).
//!
//! The id of a call is chosen by the other side, so a line is found by its CONVERSATION and
//! its id on every network, never account-wide ([`Apply::call_row`]): the row is stored under
//! the per-conversation key (`net_scope = 1`, index `messages_key_conv`) also on a network
//! whose message ids are account-wide. The same id in two chats is two lines that do not see
//! each other; so are the memory of a removed line ([`crate::expiry::remembered`]) and the
//! modifiers that wait for one (`Apply::apply_pending`).
//!
//! Who may say what ([`CallLine::may_come_from_peer`]): a signal of the other side says only
//! that an incoming call was started, rings or was missed; anything else from it is refused
//! as a contract violation, here as in `Envelope::validate`. Answered, declined, unanswered,
//! every signal of an outgoing call and the delete of a line come from our own devices only.
//!
//! A ring's deadline is kept on the local clock and capped
//! ([`CallLine::on_local_clock`]): the time the signal says it has left, at most
//! [`kourier_model::MAX_RING_MS`], counted from the signal's `received_at`.
//!
//! The row has no author (`net_author = ''`), like a notice of our own doing: the phone's sync
//! of a group call names nobody, so the key cannot hold who rang. It is not a message we
//! sent: it has no outgoing state, no send state and no receipts, reading it queues no
//! receipt and no read sync, and deleting it for me queues no delete sync
//! ([`is_call_line`]). Its `origin` column says which way the call went (`Remote` for an
//! incoming one), which is what the timeline's unread marker goes by; what is left of a
//! deleted line has one fixed value there ([`DEAD_ORIGIN`]).
//!
//! Disappearing messages: a live line takes the chat's timer like a message that is read at
//! once. The timer starts when the line is stored and is not started again by a later signal;
//! the expiry sweeper removes the row like any other, and a signal that comes after that does
//! not bring it back. A line of a HISTORY batch takes the timer its signal states
//! (`IncomingMessage.expiry`), as an imported message does, and never the chat's: a start
//! that is given stands; without one an `AfterSend` timer counts from the call's time and an
//! `AfterRead` one from the reading of the line, which for a line that is not unread is
//! the moment it is stored.
//!
//! Unread: a line has a read state only while [`CallLine::counts_unread`] holds: `Unread`
//! until somebody reads it. It becomes unread again only by MOVING into such a state, so a
//! repeated signal never undoes a read. A line that moves on (answered, declined) has none
//! and counts no more, and the desktop takes its notification back (`kourier-app`
//! `policy::changed_since_shown`). A ringing line has none either (the phone rings);
//! [`Apply::settle_calls`] turns one past its deadline into a missed one with the same
//! effects as a signal. A line of a history batch follows the same rule by its `read` flag:
//! a missed call the phone had not seen (`read: false`) is unread and counts, one it had
//! seen (`read: true`) is read. It is never announced, whatever it says.
//!
//! Announcing: a line that becomes a missed call by a signal is announced like a message. One
//! that the CLOCK makes missed is announced at once only when that happens on time and the
//! account is connected and caught up; when it happens well after the deadline
//! ([`SETTLED_LATE_MS`]: Kourier was closed or the machine slept) or before the account's
//! catch-up is done, the phone's sync may still say the call was answered, so the line waits
//! for the catch-up summary ([`super::Changes::held_calls`], `crate::calls`).

use kourier_backend_api::{ContractViolation, IncomingMessage};
use kourier_db::rusqlite::{Connection, OptionalExtension, params};
use kourier_model::{
    Body, CallDirection, CallLine, ConversationId, DisappearKind, MessageId, MsgIdScope, Origin,
    SystemEvent,
};

use super::message::MsgRow;
use super::{Apply, NotifyCandidate};
use crate::error::{CoreError, Result};
use crate::sql;

/// How long after its deadline (ms) a ring may be settled and still count as settled on
/// time. Later than that the deadline task was not awake when it came (Kourier was closed,
/// the machine slept): the missed call is old news and the phone's sync may still say that
/// it was answered, so it is not announced alone.
pub(crate) const SETTLED_LATE_MS: i64 = 10_000;

/// The `net_scope` of every call line: the per-conversation key, on every network.
const LINE_SCOPE: MsgIdScope = MsgIdScope::Conversation;

/// The `origin` column of what is left of a deleted line: one value, whatever the line was.
pub(crate) const DEAD_ORIGIN: &str = "OwnOtherDevice";

/// How a change of a line may be announced.
#[derive(Clone, Copy, Debug)]
struct Announce {
    /// Not at all (history, a silent signal).
    silent: bool,
    /// As part of the catch-up summary instead of alone.
    catch_up: bool,
    /// The signal says the line is read already: it does not become unread.
    read: bool,
    /// Not alone: it waits for the catch-up summary, and counts there only if it is still
    /// a missed call then (the clock made it one late, or before the catch-up was done).
    held: bool,
}

/// Whether a line that rang until `deadline` and is settled at `now` is settled late.
fn late(deadline: Option<i64>, now: i64) -> bool {
    deadline.is_some_and(|d| now.saturating_sub(d) > SETTLED_LATE_MS)
}

/// The `origin` column of a line: which way the call went.
fn origin_name(line: &CallLine) -> &'static str {
    match line.direction {
        CallDirection::Incoming => "Remote",
        CallDirection::Outgoing => "OwnOtherDevice",
    }
}

/// The stored call line of message `id`, when it is one (and not a tombstone).
pub(crate) fn stored_line(conn: &Connection, id: MessageId) -> Result<Option<CallLine>> {
    let body: Option<String> = conn
        .prepare_cached(
            "SELECT body FROM messages WHERE id = ?1 AND body_kind = 'System' AND deleted IS NULL",
        )?
        .query_row([id.0], |r| r.get(0))
        .optional()?;
    Ok(
        match body.map(|b| sql::from_json::<Body>(&b)).transpose()? {
            Some(Body::System(SystemEvent::Call(line))) => Some(line),
            _ => None,
        },
    )
}

/// Whether message `id` is a call line: nothing of it is ever sent.
pub(crate) fn is_call_line(conn: &Connection, id: MessageId) -> Result<bool> {
    Ok(stored_line(conn, id)?.is_some())
}

impl Apply<'_> {
    /// The row under the id `local` of a call line in `conv`: the line, what is left of a
    /// deleted one, or (on a network whose message ids count per conversation) a message that
    /// happens to have that id. Never a row of another conversation.
    pub(crate) fn call_row(&self, conv: ConversationId, local: &str) -> Result<Option<MsgRow>> {
        let id: Option<i64> = self
            .conn
            .prepare_cached(
                "SELECT id FROM messages WHERE conversation_id = ?1 AND net_local = ?2
                   AND net_scope = 1",
            )?
            .query_row((conv.0, local), |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => self.msg_row(MessageId(id)),
            None => Ok(None),
        }
    }

    /// Ingests one signal of a call: makes its line, or merges into the line that is there.
    pub(crate) fn call_signal(
        &mut self,
        m: IncomingMessage,
        line: CallLine,
        history: bool,
    ) -> Result<()> {
        let key = self.canon_conv(&m.conv)?;
        // Who says it: the other side of the call, or one of our own devices (a remote
        // origin naming one of our ids is our own, contract §10.2).
        let from_peer = match &m.origin {
            Origin::Remote { author, .. } => {
                let author = self.resolve_user(author)?;
                !self.is_own(&author)
            }
            Origin::OwnThisDevice | Origin::OwnOtherDevice { .. } => false,
        };
        if from_peer && !line.may_come_from_peer() {
            // `Envelope::validate` refuses it before it gets here; whatever calls ingest
            // without it gets the same answer.
            return Err(CoreError::Contract(ContractViolation::CallLine(
                "the other side of a call may only say that an incoming call was started, \
                 rings or was missed",
            )));
        }
        // The ring's deadline on our clock, capped.
        let mut line = line.on_local_clock(m.sent_at, m.received_at);
        // Who rang, under their canonical id; one of our own ids is nobody else.
        if let Some(by) = line.by.take() {
            let by = self.resolve_user(&by)?;
            if !self.is_own(&by) {
                line.by = Some(by);
            }
        }
        let line = line.normalized();
        let mut how = Announce {
            silent: history || m.silent,
            catch_up: m.catch_up,
            read: m.read,
            held: false,
        };
        let found = match sql::conversation_id(self.conn, self.acc.id, &key)? {
            Some(c) => self.call_row(c, &m.local)?,
            None => None,
        };
        if let Some(ex) = found {
            let stored = stored_line(self.conn, ex.id)?;
            if stored.is_none() && ex.deleted.is_none() {
                // A row that is no call line: not ours to change.
                return Ok(());
            }
            // (Also for a deleted line, so that what is left of it does not depend on
            // whether its delete came before or after this signal. A line that came with
            // imported history is a live one once a live signal names it.)
            self.conn
                .prepare_cached(
                    "UPDATE messages SET sent_at = min(sent_at, ?2),
                       received_at = min(received_at, ?3),
                       server_at = CASE WHEN server_at IS NULL THEN ?4
                         WHEN ?4 IS NULL THEN server_at ELSE min(server_at, ?4) END,
                       arrival = CASE WHEN arrival IS NULL THEN ?5
                         WHEN ?5 IS NULL THEN arrival ELSE min(arrival, ?5) END,
                       silent = min(silent, ?6), history = min(history, ?7)
                     WHERE id = ?1",
                )?
                .execute(params![
                    ex.id.0,
                    m.sent_at,
                    m.received_at,
                    m.server_at,
                    m.arrival.map(sql::i64_of),
                    how.silent,
                    history,
                ])?;
            // A deleted line stays deleted.
            let Some(stored) = stored else {
                return Ok(());
            };
            let earlier = m.sent_at < ex.sent_at;
            let joined = stored.join(&line);
            let settled = joined.settled(self.now);
            // The clock, not this signal, makes it a missed call, and does so late.
            how.held = settled != joined && late(joined.deadline(), self.now);
            return self.store_line(&ex, &stored, &settled, how, earlier);
        }

        // On a network with account-wide ids a message of ours may carry the id in this
        // chat: the line does not take its place.
        if self.scope == MsgIdScope::AccountAuthor
            && self
                .by_key(ConversationId(0), "", &m.local)?
                .is_some_and(|r| r.conv_key == key.encode())
        {
            return Ok(());
        }
        // The line went with a cleared chat, or its timer ran out: a later signal of the same
        // call does not bring it back (as a copy of any removed message,
        // [`crate::expiry::remembered`], by this chat and this id), and leaves no empty chat
        // behind.
        if crate::expiry::remembered(
            self.conn,
            self.acc.id,
            LINE_SCOPE,
            &key.encode(),
            "",
            &m.local,
            self.now,
        )? {
            tracing::debug!(
                account = self.acc.id.0,
                "a signal of a call whose line was removed was dropped"
            );
            return Ok(());
        }
        let conv = self.ensure_conversation(&key)?;
        let settled = line.settled(self.now);
        how.held = settled != line && late(line.deadline(), self.now);
        let line = settled;
        let sort_key = self.sort_key_of(conv, &m, history)?;
        let (read_elsewhere, timer_secs): (bool, Option<i64>) = self
            .conn
            .prepare_cached(
                "SELECT coalesce(read_sent_at >= ?2, 0), timer_secs FROM conversations
                 WHERE id = ?1",
            )?
            .query_row((conv.0, m.sent_at), |r| Ok((r.get(0)?, r.get(1)?)))?;
        let read_state = line
            .counts_unread()
            .then_some(if how.read || read_elsewhere {
                "Read"
            } else {
                "Unread"
            });
        let (kind, timer_ms, started_at) = if history {
            // The timer the import states for the line, as for an imported message. One
            // that starts at reading starts now unless the line is unread: nothing else
            // would ever start it.
            match m.expiry.filter(|e| !e.timer.is_zero()) {
                Some(e) => {
                    let started = e.started_at.or(match e.kind {
                        DisappearKind::AfterSend => Some(m.sent_at),
                        DisappearKind::AfterRead if read_state == Some("Unread") => None,
                        DisappearKind::AfterRead => Some(self.now),
                    });
                    let ms = i64::try_from(e.timer.as_millis()).unwrap_or(i64::MAX);
                    (Some(e.kind), Some(ms), started)
                }
                None => (None, None, None),
            }
        } else {
            // The chat's disappearing timer, as for a message that is read at once: it
            // starts now, once.
            let ms = timer_secs
                .filter(|s| *s > 0)
                .map(|s| s.saturating_mul(1000));
            (
                ms.map(|_| DisappearKind::AfterRead),
                ms,
                ms.map(|_| self.now),
            )
        };
        let expiry_kind = kind.map(|k| sql::enum_name(&k)).transpose()?;
        let expires_at = started_at.zip(timer_ms).map(|(s, ms)| s.saturating_add(ms));
        let body = Body::System(SystemEvent::Call(line.clone()));
        self.conn
            .prepare_cached(
                "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
                   origin, sort_key, sent_at, server_at, received_at, arrival, body_kind, body,
                   read_state, history, silent, call_deadline, expiry_kind, expiry_timer_ms,
                   expiry_started_at, expires_at)
                 VALUES (?1, ?2, ?3, '', ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'System', ?11, ?12, ?13, ?14,
                   ?15, ?16, ?17, ?18, ?19)",
            )?
            .execute(params![
                self.acc.id.0,
                conv.0,
                LINE_SCOPE.db_code(),
                m.local.as_ref(),
                origin_name(&line),
                sort_key,
                m.sent_at,
                m.server_at,
                m.received_at,
                m.arrival.map(sql::i64_of),
                sql::to_json(&body)?,
                read_state,
                history,
                how.silent,
                line.deadline(),
                expiry_kind,
                timer_ms,
                started_at,
                expires_at,
            ])?;
        let id = MessageId(self.conn.last_insert_rowid());
        // A chat deleted on another device comes back with its next call, as with a message.
        self.unhide(conv)?;
        self.ch.message(conv, id);
        self.ch.ringing |= line.deadline().is_some();
        self.ch.expiring |= expires_at.is_some();
        // A delete of the call entry that came first (the phone deleted it before we saw it).
        if let Some(row) = self.msg_row(id)? {
            self.apply_pending(&row, &row.net_local.clone())?;
        }
        self.announce(conv, id, how)
    }

    /// Writes `new` into the row of a line that held `old`, and what follows from the change.
    fn store_line(
        &mut self,
        row: &MsgRow,
        old: &CallLine,
        new: &CallLine,
        how: Announce,
        moved: bool,
    ) -> Result<()> {
        if new == old {
            if moved {
                // It began earlier than we knew: the time it shows changed.
                self.read_elsewhere(row.id)?;
                self.ch.message(row.conv, row.id);
            }
            return Ok(());
        }
        let counts = new.counts_unread();
        // Unread again only by moving into a state that counts. A line in any other state
        // has no read state at all (so the same signals in another order, where it never
        // counted, leave the same row).
        let became = counts && !(old.counts_unread() && old.state == new.state);
        let read_state = match (counts, row.read_state.as_deref()) {
            (false, _) => None,
            (true, _) if became && !how.read => Some("Unread"),
            (true, None) => Some("Read"),
            (true, was) => was,
        };
        let body = Body::System(SystemEvent::Call(new.clone()));
        self.conn
            .prepare_cached(
                "UPDATE messages SET body = ?2, origin = ?3, read_state = ?4, call_deadline = ?5
                 WHERE id = ?1",
            )?
            .execute(params![
                row.id.0,
                sql::to_json(&body)?,
                origin_name(new),
                read_state,
                new.deadline(),
            ])?;
        self.ch.message(row.conv, row.id);
        self.ch.ringing |= new.deadline().is_some();
        self.read_elsewhere(row.id)?;
        if read_state != Some("Unread") {
            // A line that is not unread (any more) is read: a timer that waited for the
            // reading starts (only an imported line can have one that did not start).
            self.start_read_timer(row.id)?;
        }
        if became {
            self.announce(row.conv, row.id, how)?;
        }
        Ok(())
    }

    /// The read marker of our other devices is a `sent_at` watermark (§4.9): an unread line
    /// that began at or before it is read, whenever the signal that says so arrives.
    fn read_elsewhere(&mut self, id: MessageId) -> Result<()> {
        let read = self
            .conn
            .prepare_cached(
                "UPDATE messages SET read_state = 'Read'
                 WHERE id = ?1 AND read_state = 'Unread' AND sent_at <=
                   (SELECT c.read_sent_at FROM conversations c WHERE c.id = conversation_id)",
            )?
            .execute([id.0])?;
        if read > 0 {
            self.start_read_timer(id)?;
        }
        Ok(())
    }

    /// The notification decision for a line that just became unread (§8.7), as for a message:
    /// not for a blocked or muted chat, and as part of the summary during a catch-up.
    fn announce(&mut self, conv: ConversationId, id: MessageId, how: Announce) -> Result<()> {
        if how.silent {
            return Ok(());
        }
        let unread = self
            .msg_row(id)?
            .is_some_and(|r| r.read_state.as_deref() == Some("Unread") && r.deleted.is_none());
        if !unread {
            return Ok(());
        }
        let cs = self.conv_state(conv)?;
        if cs.blocked {
            return Ok(());
        }
        let candidate = NotifyCandidate {
            conv,
            message: id,
            mentions_me: false,
        };
        if how.catch_up {
            self.ch.catch_up.push(conv);
        } else if !self.may_notify(&cs, false) {
            // Muted: unread, and nobody is told.
        } else if how.held {
            self.ch.held_calls.push(candidate);
        } else {
            self.ch.notify.push(candidate);
        }
        Ok(())
    }

    /// The deadline: every line of this account that still rings at `self.now` past its
    /// deadline becomes a missed one, with what an ingested signal would change (the row, the
    /// counters, a notification). `caught_up` says whether the account is connected and done
    /// with its catch-up: only then, and only when the deadline is not long past, is the
    /// missed call announced at once; otherwise it waits for the catch-up summary
    /// ([`super::Changes::held_calls`]). Returns how many lines changed.
    pub(crate) fn settle_calls(&mut self, caught_up: bool) -> Result<usize> {
        let due: Vec<i64> = self
            .conn
            .prepare_cached(
                "SELECT id FROM messages INDEXED BY messages_ringing
                 WHERE call_deadline IS NOT NULL AND call_deadline <= ?1 AND +account_id = ?2
                 ORDER BY call_deadline, id",
            )?
            .query_map((self.now, self.acc.id.0), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let mut changed = 0;
        for id in due {
            let id = MessageId(id);
            let (Some(row), Some(old)) = (self.msg_row(id)?, stored_line(self.conn, id)?) else {
                // A tombstone, or a row that is no call line: it has no deadline.
                self.conn
                    .prepare_cached("UPDATE messages SET call_deadline = NULL WHERE id = ?1")?
                    .execute([id.0])?;
                continue;
            };
            let new = old.settled(self.now);
            let how = Announce {
                silent: self.is_silent(id)?,
                catch_up: false,
                read: false,
                held: !caught_up || late(old.deadline(), self.now),
            };
            if new == old {
                // The body does not ring (any more): only the column was left behind.
                self.conn
                    .prepare_cached("UPDATE messages SET call_deadline = ?2 WHERE id = ?1")?
                    .execute((id.0, new.deadline()))?;
                continue;
            }
            self.store_line(&row, &old, &new, how, false)?;
            changed += 1;
        }
        Ok(changed)
    }

    fn is_silent(&self, id: MessageId) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT silent OR history FROM messages WHERE id = ?1")?
            .query_row([id.0], |r| r.get(0))?)
    }
}
