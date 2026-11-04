//! The re-key of an own message (KT-2, architecture §3.1, §4.8): a network whose server names
//! messages answers a send with an id of its own (`TransmitOutcome::SentAs`), and its echo says
//! which id the message was allocated under (`IncomingMessage::allocated_as`).
//!
//! THE RULE: a message is one row whatever it is called. The row keeps its `MessageId`, its position
//! and its files; its key becomes the server's id and the allocated id stays another name of it
//! (`message_aliases`), so a late copy, a quote or a modifier under the old id still finds it and the
//! allocator never hands the old id out again. Nothing that names the row by its rowid moves
//! (receipts, send states, reactions, links).
//!
//! A CLASH IS NEVER AN ERROR. An `Err` would roll the outcome back and leave the row `Transmitting`,
//! for a server that already has the message. A bad id, or an id that a remote row, a call line, a
//! row of another chat or an own message with its own send queued holds, is one warn line with row
//! ids only: the message keeps its allocated id and the send counts. So is an answer for a row the
//! server named already (it has a position): only the first name counts. A position out of range
//! (`MAX_NET_SEQ`) is left out and the id is taken without it. Nor does what waited under the
//! server's id fail the outcome: it applies inside a savepoint, and what cannot apply is dropped
//! with one warn line ([`Apply::settle_rekey`]), or every resend would meet it again.
//!
//! IT RUNS IN TWO PARTS around the end of the send. Part one, [`Apply::rekey_own`], before it: the
//! key, the revisions, the alias, the queue. Part two, [`Apply::settle_rekey`], after it: the floor,
//! and what waited under the server's id. What waited must find a message that is sent: until the
//! outbox row is gone the row counts as unsent (`clear::UNSENT`), so a clear that waited with the
//! server's id as its anchor would be consumed and its anchor would survive it.
//!
//! The queue: only `target.local` of an edit, a delete or a reaction and `reply.target.local` of a
//! message are rewritten. The `req.local` of a message's own row NEVER is: a resend or a "Retry" goes
//! out under the ALLOCATED id, which is what the network's map is keyed by. The quotes are found by
//! parsing every queued send of the chat that quotes ([`QUEUED_QUOTES`]) once per re-key, kept as it
//! is (review 2, L6): only the first part of a send quotes, so a send of a thousand files parses one
//! request, and a chat seldom has many quoting sends waiting at once.
//!
//! THE THREE ORDERS end as one row, with the outbox row gone:
//!
//! - outcome, then echo: the outcome re-keys and finishes; the echo finds the row under its key;
//! - echo with `allocated_as`, then outcome: the echo re-keys and finishes ([`Apply::message`]); the
//!   outcome finds the id there and records who has the message;
//! - echo without it, then outcome: two rows for a moment, and the outcome merges the echo into the
//!   message ([`Apply::merge_rows`]). When the echo was deleted meanwhile its tombstone wins, and the
//!   message is made a tombstone the way every delete makes one ([`Apply::tombstone`]): its files go
//!   and what is queued behind it does not go out, as in the two other orders.
//!
//! Only this device's echo binds (`Origin::OwnThisDevice`): another device cannot know the id this
//! one allocated.
//!
//! KNOWN LIMITS, not built: a row rewritten while it is `Transmitting` is overwritten with its
//! attempt's request when the attempt fails (only a late echo can do that, and the network's map still
//! resolves the id). Parked receipts under an alias (`retry_receipts_of`, `retry_receipts_in`) and
//! a removal that remembers the aliases of what it removes: no network path reaches them. What the
//! other side says (a receipt, a reaction) knows only the server's id, and the one message that
//! names an allocated id, the echo, names the server's beside it. Our own queue does name the
//! allocated id until the re-key (the contract lets a request name either: `TransmitOutcome::SentAs`).

use kourier_backend_api::{OutgoingKind, OutgoingRequest, is_net_seq};
use kourier_db::rusqlite::{OptionalExtension, params};
use kourier_model::ids::validate_local;
use kourier_model::{ConversationId, MessageId, MsgIdScope, TargetAuthor};

use super::Apply;
use super::message::MsgRow;
use crate::error::Result;
use crate::sql;

/// SQL: the own message that had the id `?2` under an earlier name, of the conversation `?3` when
/// one is bound (an own key under `AccountAuthor` is account-wide). Only own rows have aliases. The
/// alias drives (`CROSS JOIN`: no statistics in core.db), so a miss costs one index probe.
pub(crate) const BY_ALIAS: &str = "SELECT a.message_id FROM message_aliases a
   CROSS JOIN messages m ON m.id = a.message_id
 WHERE a.account_id = ?1 AND a.net_local = ?2 AND (?3 IS NULL OR m.conversation_id = ?3)
 LIMIT 1";

/// SQL: the queued edits, deletes and reactions of the message `?1`.
const QUEUED_CHANGES: &str = "SELECT id, request FROM outbox
 WHERE message_id = ?1 AND kind <> 'Message'";

/// SQL: the queued sends of the conversation `?1` that quote something. Only the first part of a
/// send quotes, so a send of a thousand files parses one request here, not a thousand per message.
const QUEUED_QUOTES: &str = "SELECT o.id, o.request FROM outbox o
   CROSS JOIN messages m ON m.id = o.message_id
 WHERE o.conversation_id = ?1 AND o.kind = 'Message' AND m.reply_to IS NOT NULL";

/// SQL: whether the message `?1` has a send of its own queued: it is another message of ours,
/// whatever id it has.
const HAS_QUEUED_SEND: &str =
    "SELECT EXISTS(SELECT 1 FROM outbox WHERE message_id = ?1 AND kind = 'Message')";

/// SQL: drops what waited for an own message under the id `?2` in the conversation `?3` (`?1` the
/// account, `?4` the message's author column, `''`): the rows [`Apply::apply_pending`] takes for a
/// message that is no call line (`modifier::apply_pending_depth`'s rules).
const DROP_WAITING: &str = "DELETE FROM pending_modifiers
 WHERE account_id = ?1 AND target_local = ?2
   AND (conv_key = ?3 OR (conv_key IS NULL AND target_author IS NOT NULL))
   AND (target_author IS NULL OR target_author = ?4)";

/// A message whose keys moved ([`Apply::rekey_own`]), for the part that follows the end of the send
/// ([`Apply::settle_rekey`]).
#[must_use]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rekeyed(MessageId);

/// `kind` names the id `old`: it names `new` now. Whether it did.
fn rename_target(kind: &mut OutgoingKind, old: &str, new: &str) -> bool {
    let target = match kind {
        OutgoingKind::Edit { target, .. }
        | OutgoingKind::Delete { target, .. }
        | OutgoingKind::React { target, .. } => target,
        OutgoingKind::Message {
            reply_to: Some(reply),
            ..
        } => &mut reply.target,
        OutgoingKind::Message { .. } => return false,
    };
    // (A quote of another member's message may share the id under per-sender ids.)
    if matches!(target.author, TargetAuthor::User(_)) || &*target.local != old {
        return false;
    }
    target.local = new.into();
    true
}

impl Apply<'_> {
    /// The own message that had the id `local` under an earlier name ([`BY_ALIAS`]).
    pub(super) fn by_alias(&self, conv: ConversationId, local: &str) -> Result<Option<MsgRow>> {
        let within = (self.scope != MsgIdScope::AccountAuthor).then_some(conv.0);
        let id: Option<i64> = self
            .conn
            .prepare_cached(BY_ALIAS)?
            .query_row(params![self.acc.id.0, local, within], |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => self.msg_row(MessageId(id)),
            None => Ok(None),
        }
    }

    /// Whether `row` is a call line: it has no author, and no send of ours made it.
    fn is_line(&self, row: &MsgRow) -> Result<bool> {
        Ok(row.is_own() && row.body_kind == "System" && super::is_call_line(self.conn, row.id)?)
    }

    /// Part one of the re-key (the module's rule): `message` becomes the row of the id `new`, at the
    /// position `seq` when it has one. `None` when nothing moved and nothing follows: the row is gone
    /// or no message we sent, or the id cannot be taken (one warn line). The transaction may go on
    /// either way, and the caller's send counts.
    pub(crate) fn rekey_own(
        &mut self,
        message: MessageId,
        new: &str,
        seq: Option<i64>,
    ) -> Result<Option<Rekeyed>> {
        let Some(row) = self.msg_row(message)? else {
            return Ok(None);
        };
        if !row.is_own() || self.is_line(&row)? {
            return Ok(None);
        }
        if validate_local(new).is_err() {
            tracing::warn!(
                account = self.acc.id.0,
                message = row.id.0,
                "the server gave a message an id that is no id: it keeps its allocated one"
            );
            return Ok(None);
        }
        // Positions only rise: one out of range would read the chat for good. The id goes on
        // without it; the echo states the position.
        let seq = match seq {
            Some(seq) if !is_net_seq(seq) => {
                tracing::warn!(
                    account = self.acc.id.0,
                    message = row.id.0,
                    "the server gave a message a position out of range: it takes the id without it"
                );
                None
            }
            seq => seq,
        };
        // The answer came twice, or after the echo: only a position can be new.
        if row.net_local == new {
            self.conn
                .prepare_cached(
                    "UPDATE messages SET net_seq = ?2 WHERE id = ?1 AND net_seq IS NULL AND ?2 IS NOT NULL",
                )?
                .execute(params![row.id.0, seq])?;
            return Ok(Some(Rekeyed(row.id)));
        }
        // A row with a position is one the server named already: the first name counts.
        if row.net_seq.is_some() {
            tracing::warn!(
                account = self.acc.id.0,
                message = row.id.0,
                "the server named a message it had named already: it keeps the first id"
            );
            return Ok(None);
        }
        // KEY-ONLY: an alias is no clash. Under `AccountAuthor` the key is account-wide, so the row
        // can be of another chat, and a merge must not cross chats. An own row with a send of its
        // own queued is another message of ours, not this one's echo (an allocated id never equals
        // a server's, the contract says; a merge here would lose a message).
        if let Some(other) = self.by_key_only(row.conv, "", new)? {
            if !other.is_own()
                || other.conv != row.conv
                || self.is_line(&other)?
                || self.has_queued_send(other.id)?
            {
                tracing::warn!(
                    account = self.acc.id.0,
                    message = row.id.0,
                    other = other.id.0,
                    "another row has the id the server gave a message: it keeps its allocated one"
                );
                return Ok(None);
            }
            // The echo that knew no binding: our row keeps its id, position and files.
            self.merge_rows(&row, &other)?;
            self.tombstone_if_merged_one(&row)?;
        }
        // (`OR IGNORE`: a clash the look above cannot see, a row under another scope's key, is
        // still no error.)
        let moved = self
            .conn
            .prepare_cached(
                "UPDATE OR IGNORE messages SET net_local = ?2, net_seq = coalesce(?3, net_seq)
                 WHERE id = ?1",
            )?
            .execute(params![row.id.0, new, seq])?;
        if moved == 0 {
            tracing::warn!(
                account = self.acc.id.0,
                message = row.id.0,
                "another row has the id the server gave a message: it keeps its allocated one"
            );
            return Ok(None);
        }
        // An edit made while it was unsent copied the temporary id into revision 0. A merged echo
        // can have a revision 0 under the new id already: the old one goes, or two rows without a
        // time would count as an edit nobody made.
        self.conn
            .prepare_cached(
                "UPDATE OR IGNORE message_revisions SET net_local = ?3
                 WHERE message_id = ?1 AND net_local = ?2",
            )?
            .execute((row.id.0, &row.net_local, new))?;
        self.conn
            .prepare_cached(
                "DELETE FROM message_revisions WHERE message_id = ?1 AND net_local = ?2",
            )?
            .execute((row.id.0, &row.net_local))?;
        self.reindex_revisions(row.id)?;
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_aliases(message_id, account_id, net_local)
                 VALUES (?1, ?2, ?3)",
            )?
            .execute((row.id.0, self.acc.id.0, &row.net_local))?;
        self.rewrite_targets(&row, new)?;
        self.ch.message(row.conv, row.id);
        Ok(Some(Rekeyed(row.id)))
    }

    /// Whether the message `id` has a send of its own queued ([`HAS_QUEUED_SEND`]).
    fn has_queued_send(&self, id: MessageId) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(HAS_QUEUED_SEND)?
            .query_row([id.0], |r| r.get(0))?)
    }

    /// `row` took a tombstone from the echo merged into it (`merge_rows`: a tombstone on either side
    /// wins), which moves the delete and nothing else. It becomes a tombstone the way every delete
    /// makes one: its files go and what is queued behind it does not go out, as in the orders where
    /// the delete found the message itself.
    fn tombstone_if_merged_one(&mut self, row: &MsgRow) -> Result<()> {
        let Some(kept) = self.msg_row(row.id)? else {
            return Ok(());
        };
        let Some(kind) = kept.deleted.clone().filter(|_| kept.deleted != row.deleted) else {
            return Ok(());
        };
        let by: Option<String> = self
            .conn
            .prepare_cached("SELECT deleted_by FROM messages WHERE id = ?1")?
            .query_row([kept.id.0], |r| r.get(0))?;
        self.tombstone(&kept, &kind, by.as_deref())
    }

    /// What waits in the queue behind `row` names its new id. A request that cannot be read is left
    /// alone (one warn line): the row it belongs to has its own troubles, and an `Err` here would
    /// take the outcome back.
    fn rewrite_targets(&self, row: &MsgRow, new: &str) -> Result<()> {
        let read = |sql: &str, key: i64| -> Result<Vec<(i64, String)>> {
            Ok(self
                .conn
                .prepare_cached(sql)?
                .query_map([key], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?)
        };
        let mut queued = read(QUEUED_CHANGES, row.id.0)?;
        queued.extend(read(QUEUED_QUOTES, row.conv.0)?);
        for (outbox, request) in queued {
            let Ok(mut req) = sql::from_json::<OutgoingRequest>(&request) else {
                tracing::warn!(
                    account = self.acc.id.0,
                    outbox,
                    "a queued request that cannot be read keeps the id it names"
                );
                continue;
            };
            if rename_target(&mut req.kind, &row.net_local, new) {
                self.conn
                    .prepare_cached("UPDATE outbox SET request = ?2 WHERE id = ?1")?
                    .execute((outbox, sql::to_json(&req)?))?;
            }
        }
        Ok(())
    }

    /// Part two of the re-key (the module's rule), after the end of the send. THE FLOOR: a message at
    /// or below the position the peer has read (`peer_read_seq`) is `Read`, whichever came first: the
    /// marker reached a row that had no position yet, so it moved nothing. Then what waited under the
    /// server's id applies. A row that went meanwhile leaves nothing to do.
    ///
    /// What waited applies inside a SAVEPOINT: an error there would take the outcome back, the
    /// release would queue the message again, and its resend would meet the same error, for ever.
    /// On an error the savepoint is rolled back with what it changed (the changes it told are
    /// dropped too: they would name rows, or files to unlink, that the rollback kept), what waited
    /// for the message under that id is dropped, one warn line says so, and the outcome commits.
    pub(crate) fn settle_rekey(&mut self, done: Rekeyed) -> Result<()> {
        let Rekeyed(id) = done;
        let Some(row) = self.msg_row(id)? else {
            return Ok(());
        };
        let (peer, floor): (Option<i64>, Option<String>) = self
            .conn
            .prepare_cached(
                "SELECT c.peer_read_seq, m.out_floor FROM messages m
                   JOIN conversations c ON c.id = m.conversation_id
                 WHERE m.id = ?1",
            )?
            .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if row.net_seq.zip(peer).is_some_and(|(seq, peer)| seq <= peer) {
            self.floor_read(row.conv, id, floor)?;
        }
        self.conn.execute_batch("SAVEPOINT settle_rekey")?;
        let told = std::mem::take(&mut self.ch);
        let applied = self.apply_pending(&row, &row.net_local);
        let during = std::mem::replace(&mut self.ch, told);
        match applied {
            Ok(()) => {
                self.conn.execute_batch("RELEASE settle_rekey")?;
                self.ch.recount.extend(during.recount.iter().copied());
                self.ch.receipts_to_retry |= during.receipts_to_retry;
                self.ch.absorb(during);
            }
            Err(e) => {
                self.conn
                    .execute_batch("ROLLBACK TO settle_rekey; RELEASE settle_rekey")?;
                let dropped = self.conn.prepare_cached(DROP_WAITING)?.execute((
                    self.acc.id.0,
                    &row.net_local,
                    &row.conv_key,
                    &row.net_author,
                ))?;
                tracing::warn!(
                    account = self.acc.id.0,
                    message = row.id.0,
                    dropped,
                    error = %e,
                    "what waited for a message under the server's id could not apply: dropped"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kourier_backend_api::{
        AssignedId, BackendEvent, IncomingMessage, MAX_NET_SEQ, OutgoingKind, OutgoingRequest,
        TransmitOutcome,
    };
    use kourier_db::rusqlite::Connection;
    use kourier_fake::attachments::synthetic_image;
    use kourier_fake::scripts::{own, remote, target};
    use kourier_model::{
        Body, ConvKey, ConversationId, DisappearKind, Expiry, MessageId, MsgIdScope, NetworkKind,
        Origin, OutgoingState, Text, UserId,
    };

    use super::Rekeyed;
    use crate::ingest::Apply;
    use crate::outbox::{self, Attempt};
    use crate::rules::NetRules;
    use crate::testing::IngestHarness;

    const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
    const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
    const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
    const AT: i64 = 1_700_000_000_000;
    const OLD: &str = "k0000018f00000001";
    const NEW: &str = "1760000000123";

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("user id")
    }

    fn alice() -> UserId {
        uid(ALICE)
    }

    fn chat() -> ConvKey {
        ConvKey::Direct(alice())
    }

    fn harness(dir: &Path, scope: MsgIdScope) -> IngestHarness {
        IngestHarness::open(
            &dir.join("core.db"),
            &kourier_db::DbKey::from([3u8; 32]),
            NetworkKind::Fake,
            scope,
            &[uid(ME)],
        )
        .expect("harness")
    }

    /// An own message as the core queues it.
    fn queued(conv: &ConvKey, local: &str) -> BackendEvent {
        let mut m = own(conv, local, AT, "hello");
        m.initial_out_state = Some(OutgoingState::Queued);
        BackendEvent::Message(m)
    }

    /// The echo of [`queued`]'s message under the server's id. One that names the id the message
    /// was allocated under is this device's own: only this device can know it (`message()` binds
    /// no other, R1-L1), so it says `OwnThisDevice`; one that names none is the copy of any own
    /// device.
    fn echo(conv: &ConvKey, local: &str, allocated: Option<&str>) -> IncomingMessage {
        let mut m = own(conv, local, AT, "hello");
        m.net_seq = Some(1_760_000_000_123);
        m.allocated_as = allocated.map(Into::into);
        if allocated.is_some() {
            m.origin = Origin::OwnThisDevice;
        }
        m
    }

    fn value<T: kourier_db::rusqlite::types::FromSql>(conn: &Connection, sql: &str) -> T {
        conn.query_row(sql, [], |r| r.get(0)).expect("a value")
    }

    /// The row and its conversation, where `local` is its id now.
    fn row_of(h: &IngestHarness, local: &str) -> (MessageId, ConversationId) {
        h.connection()
            .query_row(
                "SELECT id, conversation_id FROM messages WHERE net_local = ?1",
                [local],
                |r| Ok((MessageId(r.get(0)?), ConversationId(r.get(1)?))),
            )
            .expect("the row")
    }

    /// Part one of a re-key of `message`, and part two when it moved something.
    fn rekey(h: &mut IngestHarness, scope: MsgIdScope, message: MessageId, new: &str) -> bool {
        rekey_at(h, scope, message, new, 1_760_000_000_123)
    }

    /// [`rekey`] stating the position `seq`.
    fn rekey_at(
        h: &mut IngestHarness,
        scope: MsgIdScope,
        message: MessageId,
        new: &str,
        seq: i64,
    ) -> bool {
        let account = h.account();
        let tx = h.connection_mut().transaction().expect("transaction");
        let mut apply = Apply::begin(&tx, account, scope, AT).expect("begin");
        let done: Option<Rekeyed> = apply
            .rekey_own(message, new, Some(seq))
            .expect("a clash is no error");
        let moved = done.is_some();
        if let Some(done) = done {
            apply.settle_rekey(done).expect("part two");
        }
        apply.finish().expect("finish");
        drop(apply);
        tx.commit().expect("commit");
        moved
    }

    fn aliases(h: &IngestHarness, message: MessageId) -> Vec<String> {
        h.connection()
            .prepare(
                "SELECT net_local FROM message_aliases WHERE message_id = ?1 ORDER BY net_local",
            )
            .expect("prepare")
            .query_map([message.0], |r| r.get(0))
            .expect("aliases")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    /// An id that is no id, or that another row holds, moves nothing and is no error: a warn line,
    /// the message keeps its allocated id and gets no alias.
    #[test]
    fn an_id_that_cannot_be_taken_moves_nothing_and_is_no_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        h.apply(vec![
            queued(&chat(), OLD),
            BackendEvent::Message(remote(&chat(), &alice(), NEW, AT, "hers")),
        ])
        .expect("rows");
        let (message, _) = row_of(&h, OLD);
        for bad in ["", "a\u{7}b", NEW] {
            assert!(!rekey(&mut h, scope, message, bad), "{bad:?}");
            assert_eq!(row_of(&h, OLD).0, message);
            assert!(aliases(&h, message).is_empty(), "{bad:?}");
        }
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
            2,
            "the other row stands"
        );
    }

    /// Under `AccountAuthor` an own key is account-wide: the row that has the id can be of another
    /// chat, and neither moves or merges.
    #[test]
    fn a_clash_in_another_chat_merges_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::AccountAuthor;
        let mut h = harness(dir.path(), scope);
        let other = ConvKey::Direct(uid(BOB));
        h.apply(vec![queued(&chat(), OLD), queued(&other, NEW)])
            .expect("rows");
        let (message, _) = row_of(&h, OLD);
        assert!(!rekey(&mut h, scope, message, NEW));
        assert_eq!(row_of(&h, OLD).0, message);
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
            2,
            "no merge across chats"
        );
    }

    /// The echo that knew no binding joins the message: the row keeps its id and sort key, takes the
    /// position, and the echo's own aliases come with it. Mutants: `merge_rows` without the
    /// `coalesce`, without the alias move.
    #[test]
    fn an_echo_without_a_binding_joins_the_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        h.apply(vec![queued(&chat(), OLD)]).expect("the message");
        h.apply(vec![BackendEvent::Message(echo(&chat(), NEW, None))])
            .expect("the echo");
        let (message, _) = row_of(&h, OLD);
        let (echoed, _) = row_of(&h, NEW);
        let sort_key: i64 = value(h.connection(), "SELECT min(sort_key) FROM messages");
        h.connection()
            .execute(
                "INSERT INTO message_aliases(message_id, account_id, net_local) VALUES (?1, 1, 'x')",
                [echoed.0],
            )
            .expect("an alias of the echo");
        h.connection()
            .execute("UPDATE messages SET net_seq = 7 WHERE id = ?1", [echoed.0])
            .expect("a position of the echo");
        let account = h.account();
        let tx = h.connection_mut().transaction().expect("transaction");
        let mut apply = Apply::begin(&tx, account, scope, AT).expect("begin");
        let done = apply
            .rekey_own(message, NEW, None)
            .expect("no error")
            .expect("it moved");
        apply.settle_rekey(done).expect("part two");
        apply.finish().expect("finish");
        drop(apply);
        tx.commit().expect("commit");
        assert_eq!(row_of(&h, NEW).0, message, "our row has the key");
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
            1
        );
        assert_eq!(
            value::<i64>(h.connection(), "SELECT sort_key FROM messages"),
            sort_key,
            "and its position"
        );
        assert_eq!(aliases(&h, message), [OLD, "x"]);
        assert_eq!(
            value::<i64>(h.connection(), "SELECT net_seq FROM messages"),
            7,
            "the echo's position stays when the answer states none"
        );
    }

    /// An edit made while it was unsent left revision 0 under the temporary id, and the merged echo
    /// has one under the new id (edited on another device). One stays, or two rows without a time
    /// count as an edit nobody made. Mutant: the leftover not deleted.
    #[test]
    fn a_merged_echo_with_a_revision_of_its_own_leaves_no_phantom_edit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        h.apply(vec![queued(&chat(), OLD)]).expect("the message");
        h.apply(vec![BackendEvent::Message(echo(&chat(), NEW, None))])
            .expect("the echo");
        let (message, _) = row_of(&h, OLD);
        let (echoed, _) = row_of(&h, NEW);
        let body = crate::sql::to_json(&kourier_model::Body::text("x")).expect("json");
        for (id, local, at) in [
            (message, OLD, None),
            (message, "k0000018f00000002", Some(AT + 1)),
            (echoed, NEW, None),
            (echoed, "e2", Some(AT + 2)),
        ] {
            h.connection()
                .execute(
                    "INSERT INTO message_revisions(message_id, account_id, net_author, net_local,
                       revision, body, edited_at)
                     VALUES (?1, 1, '', ?2, 0, ?3, ?4)",
                    (id.0, local, &body, at),
                )
                .expect("a revision");
        }
        assert!(rekey(&mut h, scope, message, NEW));
        let revisions: Vec<(String, i64)> = h
            .connection()
            .prepare("SELECT net_local, revision FROM message_revisions ORDER BY revision")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("revisions")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            revisions,
            [
                (NEW.to_owned(), 0),
                ("k0000018f00000002".to_owned(), 1),
                ("e2".to_owned(), 2)
            ]
        );
        assert_eq!(
            value::<i64>(h.connection(), "SELECT revision FROM messages"),
            2,
            "two edits"
        );
    }

    /// The floor: a message whose position the peer has read is `Read` from part two on, one above it
    /// stays as it was. Mutant: `seq <= peer` as `seq < peer`.
    #[test]
    fn a_position_the_peer_has_read_raises_the_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        for (peer, read) in [
            (1_760_000_000_122_i64, false),
            (1_760_000_000_123, true),
            (1_760_000_000_124, true),
        ] {
            let at = dir.path().join(peer.to_string());
            std::fs::create_dir(&at).expect("dir");
            let mut h = harness(&at, scope);
            h.apply(vec![queued(&chat(), OLD)]).expect("the message");
            let (message, conv) = row_of(&h, OLD);
            h.connection()
                .execute(
                    "UPDATE conversations SET peer_read_seq = ?2 WHERE id = ?1",
                    (conv.0, peer),
                )
                .expect("the marker");
            assert!(rekey(&mut h, scope, message, NEW));
            let state: String = value(h.connection(), "SELECT out_state FROM messages");
            let state: OutgoingState = serde_json::from_str(&state).expect("a state");
            assert_eq!(state == OutgoingState::Read, read, "{peer}: {state:?}");
        }
    }

    /// An old id stays taken: the allocator goes past it, as it goes past a key.
    /// Mutant: no alias arm in `by_key`.
    #[test]
    fn the_allocator_goes_past_an_old_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let flavor = kourier_fake::Flavor::TelegramLike;
        let first = flavor.allocate(u64::try_from(AT).expect("positive")).local;
        let mut h = harness(dir.path(), scope);
        h.apply(vec![queued(&chat(), &first)]).expect("the message");
        let (message, conv) = row_of(&h, &first);
        assert!(rekey(&mut h, scope, message, NEW));
        let account = h.account();
        let net = kourier_fake::FakeNetwork::new(
            flavor,
            kourier_fake::Script::new("alloc", vec![uid(ME)]),
        );
        let tx = h.connection_mut().transaction().expect("transaction");
        let apply = Apply::begin(&tx, account, scope, AT).expect("begin");
        let id = outbox::allocate(&apply, &net, conv, &chat(), AT).expect("an id");
        assert_ne!(id.local, first, "the old id was skipped");
    }

    /// A late copy under the old id is the row; a remote message under the old id is a message of its
    /// own and pays no alias probe; `allocated_as` of a remote message means nothing.
    #[test]
    fn an_alias_is_a_name_of_an_own_row_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        h.apply(vec![queued(&chat(), OLD)]).expect("the message");
        let (message, _) = row_of(&h, OLD);
        assert!(rekey(&mut h, scope, message, NEW));
        h.apply(vec![BackendEvent::Message(own(&chat(), OLD, AT, "copy"))])
            .expect("a late copy");
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
            1,
            "the late copy found the row"
        );
        let mut hers = remote(&chat(), &alice(), OLD, AT, "hers");
        hers.allocated_as = Some(NEW.into());
        h.apply(vec![BackendEvent::Message(hers)]).expect("hers");
        let (hers, _) = row_of(&h, OLD);
        assert_ne!(hers, message, "a row of her own under that id");
        assert_eq!(row_of(&h, NEW).0, message, "and ours was not re-keyed");
    }

    /// An echo whose message is gone and remembered does not bring it back, and leaves no chat.
    /// Mutant: the look at `expiry::remembered` for the allocated id removed.
    #[test]
    fn the_echo_of_a_message_that_expired_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        let mut m = own(&chat(), OLD, AT, "hello");
        m.expiry = Some(Expiry {
            kind: DisappearKind::AfterSend,
            timer: std::time::Duration::from_secs(1),
            started_at: Some(AT),
        });
        h.apply(vec![BackendEvent::Message(m)])
            .expect("the message");
        h.set_now(AT + 60_000);
        assert_eq!(h.sweep().expect("the sweep"), 1, "it expired");
        h.apply(vec![BackendEvent::Message(echo(&chat(), NEW, Some(OLD)))])
            .expect("the echo");
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
            0
        );
    }

    /// A `Transmit` of a change answered `SentAs` is a backend fault: it reads as `Sent`, and the
    /// target keeps its key.
    #[test]
    fn a_change_answered_with_an_id_reads_as_sent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(dir.path(), MsgIdScope::Conversation);
        h.apply(vec![queued(&chat(), OLD)]).expect("the message");
        let (message, conv) = row_of(&h, OLD);
        let req = OutgoingRequest {
            conv: chat(),
            local: "k0000018f00000002".into(),
            sent_at: AT + 1,
            kind: OutgoingKind::React {
                target: target(Some(&chat()), None, OLD),
                emoji: "x".into(),
                remove: false,
            },
            only: None,
            is_recipient_update: false,
        };
        h.connection()
            .execute(
                "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
                   deadline_at) VALUES (5, ?1, ?2, 'React', ?3, ?4, 1, ?5)",
                (
                    message.0,
                    conv.0,
                    crate::sql::to_json(&req).expect("json"),
                    outbox::TRANSMITTING,
                    AT + 1000,
                ),
            )
            .expect("the row");
        let attempt = Attempt {
            outbox: 5,
            conv,
            message,
            req,
            attachments: Vec::new(),
            number: 1,
            started_at: AT,
        };
        let account = h.account();
        outbox::record_outcome(
            h.connection_mut(),
            account,
            NetRules::signal(MsgIdScope::Conversation),
            AT,
            &attempt,
            Some(TransmitOutcome::SentAs {
                assigned: AssignedId {
                    local: NEW.into(),
                    seq: 1,
                },
                server_at: Some(AT),
                per_recipient: Vec::new(),
            }),
        )
        .expect("recorded");
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM outbox"),
            0,
            "done"
        );
        assert_eq!(row_of(&h, OLD).0, message, "the target keeps its key");
    }

    /// The message `local` of [`chat`] in flight: its outbox row (5) is `Transmitting`.
    fn in_flight(h: &mut IngestHarness, local: &str) -> Attempt {
        in_flight_with(h, queued(&chat(), local), local, 5, outbox::TRANSMITTING)
    }

    /// The own message `message` (its id `local`) with its send in the outbox as the row `outbox`
    /// in `state`.
    fn in_flight_with(
        h: &mut IngestHarness,
        message: BackendEvent,
        local: &str,
        outbox: i64,
        state: &str,
    ) -> Attempt {
        h.apply(vec![message]).expect("the message");
        let (message, conv) = row_of(h, local);
        let req = OutgoingRequest {
            conv: chat(),
            local: local.into(),
            sent_at: AT,
            kind: OutgoingKind::Message {
                body: kourier_model::Body::text("hello"),
                reply_to: None,
                expiry: None,
            },
            only: None,
            is_recipient_update: false,
        };
        h.connection()
            .execute(
                "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state, attempts,
                   deadline_at) VALUES (?6, ?1, ?2, 'Message', ?3, ?4, 1, ?5)",
                (
                    message.0,
                    conv.0,
                    crate::sql::to_json(&req).expect("json"),
                    state,
                    AT + 1000,
                    outbox,
                ),
            )
            .expect("the row");
        Attempt {
            outbox,
            conv,
            message,
            req,
            attachments: Vec::new(),
            number: 1,
            started_at: AT,
        }
    }

    /// The network's rules on the fake `TelegramLike`.
    fn telegram() -> NetRules {
        NetRules::of(&kourier_fake::Flavor::TelegramLike.descriptor())
    }

    /// The answer `SentAs(new)` to `attempt`.
    fn answered(h: &mut IngestHarness, attempt: &Attempt, new: &str) {
        answered_at(h, attempt, new, 1_760_000_000_123);
    }

    /// The answer `SentAs(new)` to `attempt`, stating the position `seq`.
    fn answered_at(h: &mut IngestHarness, attempt: &Attempt, new: &str, seq: i64) {
        record(
            h,
            attempt,
            TransmitOutcome::SentAs {
                assigned: AssignedId {
                    local: new.into(),
                    seq,
                },
                server_at: Some(AT),
                per_recipient: Vec::new(),
            },
        );
    }

    /// `outcome` recorded for `attempt` under the network's rules, at `AT`.
    fn record(h: &mut IngestHarness, attempt: &Attempt, outcome: TransmitOutcome) {
        let account = h.account();
        outbox::record_outcome(
            h.connection_mut(),
            account,
            telegram(),
            AT,
            attempt,
            Some(outcome),
        )
        .expect("an outcome is recorded, whatever the id");
    }

    /// The server gives our message the id of a call line (a line's key is the chat's): no re-key, the
    /// send counts, and the echo under that id does not touch the line or fail its envelope.
    #[test]
    fn the_id_of_a_call_line_is_no_error_and_the_send_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        let ring = kourier_fake::calls::Call::direct(&alice(), 7, kourier_model::CallMedia::Voice)
            .ring(AT);
        let BackendEvent::Message(line) = &ring else {
            panic!("a ring is a message");
        };
        let taken = line.local.clone();
        h.apply(vec![ring]).expect("the line");
        let attempt = in_flight(&mut h, OLD);
        h.apply(vec![BackendEvent::Message(echo(
            &chat(),
            &taken,
            Some(OLD),
        ))])
        .expect("the echo fails nothing");
        answered(&mut h, &attempt, &taken);
        assert_eq!(row_of(&h, OLD).0, attempt.message, "it keeps its id");
        assert!(aliases(&h, attempt.message).is_empty());
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM outbox"),
            0,
            "the send counts"
        );
        let floor: String = h
            .connection()
            .query_row(
                "SELECT out_floor FROM messages WHERE id = ?1",
                [attempt.message.0],
                |r| r.get(0),
            )
            .expect("the floor");
        assert!(floor.starts_with("{\"Sent\""), "{floor}");
        assert_eq!(
            value::<i64>(
                h.connection(),
                "SELECT count(*) FROM messages WHERE net_local LIKE 'call:%'"
            ),
            1,
            "the line stands"
        );
    }

    /// A delete that waited under the server's id applies through the ANSWER, under the network's
    /// rules. Mutant: `record_outcome` under the default rules (the old rule consumes it and it is
    /// lost: Telegram states `delete_receive: None`).
    #[test]
    fn a_delete_that_waited_applies_through_the_answer_under_the_networks_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(dir.path(), MsgIdScope::Conversation);
        let mut features = kourier_fake::scripts::signal_features(false);
        features.delete_receive = None;
        let mut upsert = kourier_model::ConversationPatch::new(chat());
        upsert.features = Some(features);
        h.apply(vec![BackendEvent::ConversationUpsert(upsert)])
            .expect("the chat");
        let attempt = in_flight(&mut h, OLD);
        h.apply(vec![BackendEvent::Deleted {
            target: target(Some(&chat()), None, NEW),
            by: None,
            for_everyone: true,
            at: AT,
        }])
        .expect("it waits");
        answered(&mut h, &attempt, NEW);
        assert_eq!(
            value::<String>(h.connection(), "SELECT deleted FROM messages"),
            "ForEveryone"
        );
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM pending_modifiers"),
            0
        );
    }

    /// A clear that waited with the server's id as its anchor finds a message that is sent, in both
    /// ways the re-key comes: until the outbox row is gone the message counts as unsent and the clear
    /// would be consumed while its anchor survives. Mutant: part two before the end of the send, in
    /// `record_outcome` or in `message()`.
    #[test]
    fn a_clear_that_waited_for_the_servers_id_takes_its_anchor() {
        let scope = MsgIdScope::Conversation;
        for through_the_echo in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut h = harness(dir.path(), scope);
            let attempt = in_flight(&mut h, OLD);
            h.apply(vec![BackendEvent::ConversationCleared {
                conv: chat(),
                up_to: vec![target(Some(&chat()), None, NEW)],
                remove: false,
                before_ms: None,
                at: AT,
            }])
            .expect("it waits");
            assert_eq!(
                value::<i64>(h.connection(), "SELECT count(*) FROM pending_modifiers"),
                1
            );
            if through_the_echo {
                h.apply(vec![BackendEvent::Message(echo(&chat(), NEW, Some(OLD)))])
                    .expect("the echo");
            } else {
                answered(&mut h, &attempt, NEW);
            }
            assert_eq!(
                value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
                0,
                "through the echo: {through_the_echo}"
            );
            assert_eq!(
                value::<i64>(h.connection(), "SELECT count(*) FROM pending_modifiers"),
                0
            );
        }
    }

    /// What the re-key reads of the queue starts from the message or the conversation: a send of
    /// thousands of files is thousands of rows of one chat, and every one of them is re-keyed.
    #[test]
    fn the_looks_at_the_queue_start_from_an_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = harness(dir.path(), MsgIdScope::Conversation);
        for (sql, want) in [
            (super::QUEUED_CHANGES, "outbox_by_message (message_id=?)"),
            (
                super::QUEUED_QUOTES,
                "outbox_by_conversation (conversation_id=?)",
            ),
        ] {
            let steps: Vec<String> = h
                .connection()
                .prepare(&format!("EXPLAIN QUERY PLAN {}", sql.replace("?1", "1")))
                .expect("prepare")
                .query_map([], |r| r.get(3))
                .expect("plan")
                .collect::<Result<_, _>>()
                .expect("rows");
            assert!(steps.iter().any(|s| s.contains(want)), "{sql}: {steps:#?}");
            assert!(
                steps.iter().all(|s| !s.contains("SCAN")),
                "{sql}: {steps:#?}"
            );
        }
    }

    /// The echo that knew no binding was deleted for everyone before the outcome merged it into our
    /// row (R1-M1): the tombstone wins the merge and our row becomes a tombstone THE WAY EVERY
    /// DELETE MAKES ONE, its files and attachments go and the reaction queued behind it does not go
    /// out, exactly as when the delete comes after the outcome and finds our row itself. Both orders
    /// end in the same place. Mutant: the line `self.tombstone_if_merged_one(&row)?;` of
    /// `rekey_own` deleted.
    #[test]
    fn an_echo_deleted_before_the_answer_makes_our_row_a_tombstone_as_every_delete_does() {
        let mut ends = Vec::new();
        for delete_first in [true, false] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut h = harness(dir.path(), MsgIdScope::Conversation)
                .with_server(kourier_fake::Flavor::TelegramLike.descriptor().server);
            let mut photo = own(&chat(), OLD, AT, "");
            photo.body = Body::Media {
                attachments: vec![synthetic_image(900, 600, 3)],
                caption: Some(Text::plain("hello")),
            };
            photo.initial_out_state = Some(OutgoingState::Queued);
            let attempt = in_flight_with(
                &mut h,
                BackendEvent::Message(photo),
                OLD,
                5,
                outbox::TRANSMITTING,
            );
            // Its file, and a reaction queued behind it.
            h.connection()
                .execute(
                    "INSERT INTO files(id, account_id, path, key, purpose)
                     VALUES (1, 1, 'a/photo', x'00', 'attachment')",
                    [],
                )
                .expect("a file");
            h.connection()
                .execute("UPDATE attachments SET file_id = 1", [])
                .expect("its file");
            let react = OutgoingRequest {
                conv: chat(),
                local: "k0000018f00000003".into(),
                sent_at: AT + 1,
                kind: OutgoingKind::React {
                    target: target(Some(&chat()), None, OLD),
                    emoji: "x".into(),
                    remove: false,
                },
                only: None,
                is_recipient_update: false,
            };
            h.connection()
                .execute(
                    "INSERT INTO outbox(id, message_id, conversation_id, kind, request, state,
                       attempts, deadline_at) VALUES (6, ?1, ?2, 'React', ?3, ?4, 0, ?5)",
                    (
                        attempt.message.0,
                        attempt.conv.0,
                        crate::sql::to_json(&react).expect("json"),
                        outbox::QUEUED,
                        AT + 1000,
                    ),
                )
                .expect("the reaction");
            let count = |h: &IngestHarness, sql: &str| value::<i64>(h.connection(), sql);
            assert_eq!(count(&h, "SELECT count(*) FROM attachments"), 1);
            assert_eq!(count(&h, "SELECT count(*) FROM files"), 1);
            assert_eq!(
                count(&h, "SELECT count(*) FROM outbox WHERE kind = 'React'"),
                1
            );
            let echoed = BackendEvent::Message(echo(&chat(), NEW, None));
            let delete = BackendEvent::Deleted {
                target: target(Some(&chat()), None, NEW),
                by: None,
                for_everyone: true,
                at: AT,
            };
            if delete_first {
                h.apply(vec![echoed]).expect("the echo");
                h.apply(vec![delete]).expect("the delete of the echo");
                assert_eq!(
                    count(&h, "SELECT count(*) FROM messages"),
                    2,
                    "not merged yet"
                );
                answered(&mut h, &attempt, NEW);
            } else {
                answered(&mut h, &attempt, NEW);
                h.apply(vec![echoed]).expect("the echo");
                h.apply(vec![delete]).expect("the delete");
            }
            assert_eq!(row_of(&h, NEW).0, attempt.message, "one row, ours");
            ends.push((
                count(&h, "SELECT count(*) FROM messages"),
                value::<String>(h.connection(), "SELECT deleted FROM messages"),
                value::<String>(h.connection(), "SELECT body_kind FROM messages"),
                count(&h, "SELECT count(*) FROM attachments"),
                count(&h, "SELECT count(*) FROM files"),
                count(&h, "SELECT count(*) FROM outbox WHERE kind = 'React'"),
                count(&h, "SELECT count(*) FROM outbox"),
                aliases(&h, attempt.message),
            ));
        }
        assert_eq!(
            ends[0],
            (
                1,
                "ForEveryone".to_owned(),
                "Text".to_owned(),
                0,
                0,
                0,
                0,
                vec![OLD.to_owned()]
            ),
            "the delete came first: a tombstone with no file and nothing queued behind it"
        );
        assert_eq!(
            ends[0], ends[1],
            "and the same as when the delete came last"
        );
    }

    /// A position out of range in the answer (R1-M2) is left out: the id is taken without it, the
    /// row keeps no `net_seq` (a later echo states it) and the send counts. The ends of the range
    /// pass. Mutant: the out-of-range arm of `rekey_own` passes the position through.
    #[test]
    fn a_position_out_of_range_in_the_answer_is_left_out_and_the_id_is_taken() {
        for (seq, stored) in [
            (-1, None),
            (MAX_NET_SEQ + 1, None),
            (i64::MIN, None),
            (i64::MAX, None),
            (0, Some(0)),
            (MAX_NET_SEQ, Some(MAX_NET_SEQ)),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut h = harness(dir.path(), MsgIdScope::Conversation);
            let attempt = in_flight(&mut h, OLD);
            answered_at(&mut h, &attempt, NEW, seq);
            assert_eq!(row_of(&h, NEW).0, attempt.message, "{seq}: re-keyed");
            assert_eq!(aliases(&h, attempt.message), [OLD], "{seq}");
            assert_eq!(
                value::<Option<i64>>(h.connection(), "SELECT net_seq FROM messages"),
                stored,
                "{seq}"
            );
            assert_eq!(
                value::<i64>(h.connection(), "SELECT count(*) FROM outbox"),
                0,
                "{seq}: the send counts"
            );
            let floor: String = value(h.connection(), "SELECT out_floor FROM messages");
            assert!(floor.starts_with("{\"Sent\""), "{seq}: {floor}");
            if stored.is_none() {
                h.apply(vec![BackendEvent::Message(echo(&chat(), NEW, None))])
                    .expect("the echo");
                assert_eq!(
                    value::<Option<i64>>(h.connection(), "SELECT net_seq FROM messages"),
                    Some(1_760_000_000_123),
                    "{seq}: the echo states it"
                );
            }
        }
    }

    /// An echo from ANOTHER own device that states `allocated_as` re-keys nothing (R1-L1): that
    /// device cannot know what this one allocated. Our queued row keeps its id and gets no alias;
    /// the copy is a row of its own. The same echo from this device re-keys. Mutant: the
    /// `matches!(origin, Origin::OwnThisDevice) &&` guard in `message()` dropped.
    #[test]
    fn an_echo_from_another_device_that_names_an_allocated_id_rekeys_nothing() {
        for this_device in [false, true] {
            let dir = tempfile::tempdir().expect("tempdir");
            let scope = MsgIdScope::Conversation;
            let mut h = harness(dir.path(), scope);
            h.apply(vec![queued(&chat(), OLD)]).expect("the message");
            let (message, _) = row_of(&h, OLD);
            let mut copy = echo(&chat(), NEW, Some(OLD));
            copy.origin = if this_device {
                Origin::OwnThisDevice
            } else {
                Origin::OwnOtherDevice { device: Some(2) }
            };
            h.apply(vec![BackendEvent::Message(copy)])
                .expect("the echo");
            let rows = value::<i64>(h.connection(), "SELECT count(*) FROM messages");
            if this_device {
                assert_eq!(row_of(&h, NEW).0, message, "this device's echo re-keys");
                assert_eq!(aliases(&h, message), [OLD]);
                assert_eq!(rows, 1);
            } else {
                assert_eq!(row_of(&h, OLD).0, message, "ours keeps its allocated id");
                assert!(aliases(&h, message).is_empty(), "and has no alias");
                assert_eq!(
                    value::<Option<i64>>(
                        h.connection(),
                        &format!("SELECT net_seq FROM messages WHERE id = {}", message.0)
                    ),
                    None,
                    "and no position"
                );
                assert_ne!(row_of(&h, NEW).0, message, "the copy is a row of its own");
                assert_eq!(rows, 2);
            }
        }
    }

    /// A second answer with ANOTHER id for a row the server named already (it has a position)
    /// changes nothing (R1-L1): only the first name counts, no alias is added. Mutant: the early
    /// return of `rekey_own` on `row.net_seq.is_some()` removed.
    #[test]
    fn a_second_answer_with_another_id_for_a_row_the_server_named_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        let attempt = in_flight(&mut h, OLD);
        answered(&mut h, &attempt, NEW);
        assert_eq!(row_of(&h, NEW).0, attempt.message);
        assert!(
            !rekey(&mut h, scope, attempt.message, "1760000000999"),
            "nothing moved"
        );
        assert_eq!(row_of(&h, NEW).0, attempt.message, "the first id stands");
        assert_eq!(aliases(&h, attempt.message), [OLD], "and no alias is added");
        assert_eq!(
            value::<i64>(
                h.connection(),
                "SELECT count(*) FROM messages WHERE net_local = '1760000000999'"
            ),
            0
        );
    }

    /// The server's id is held by ANOTHER own message of the chat that has a send of its own queued
    /// (R1-L1; R2-M5's scenario): that is not this message's echo, a merge would lose a message. No
    /// merge, no re-key, both rows stay and the send counts: the answered message's outbox row is
    /// gone, the other's is not. Mutant: the clause `|| self.has_queued_send(other.id)?` dropped.
    #[test]
    fn the_id_of_another_own_message_with_a_send_queued_moves_nothing_and_the_send_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(dir.path(), MsgIdScope::Conversation);
        let first = in_flight(&mut h, OLD);
        let second = in_flight_with(&mut h, queued(&chat(), NEW), NEW, 6, outbox::QUEUED);
        answered(&mut h, &first, NEW);
        assert_eq!(row_of(&h, OLD).0, first.message, "it keeps its id");
        assert_eq!(row_of(&h, NEW).0, second.message, "and the other its own");
        assert!(aliases(&h, first.message).is_empty());
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
            2,
            "no merge"
        );
        let left: Vec<i64> = h
            .connection()
            .prepare("SELECT id FROM outbox")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("ids");
        assert_eq!(
            left,
            [6],
            "the send counts: only the other message's row is left"
        );
        let floor: String = h
            .connection()
            .query_row(
                "SELECT out_floor FROM messages WHERE id = ?1",
                [first.message.0],
                |r| r.get(0),
            )
            .expect("the floor");
        assert!(floor.starts_with("{\"Sent\""), "{floor}");
    }

    /// What waited under the server's id fails to apply (R1-L4): the outcome still commits. The row
    /// is re-keyed, the outbox row is gone, the parked row is dropped (and not left to meet every
    /// resend), and `record_outcome` answers `Ok` (`answered` expects it). Mutant: the `match
    /// applied { .. }` of `settle_rekey` replaced by `applied?;`, which sends the error out and
    /// rolls the outcome back.
    #[test]
    fn what_waited_under_the_servers_id_and_cannot_apply_is_dropped_and_the_outcome_commits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(dir.path(), MsgIdScope::Conversation);
        let attempt = in_flight(&mut h, OLD);
        let conv_key: String = value(h.connection(), "SELECT conv_key FROM conversations");
        h.connection()
            .execute(
                "INSERT INTO pending_modifiers(account_id, conv_key, target_author, target_local,
                   kind, payload, dedup, created_at, expires_at)
                 VALUES (1, ?1, '', ?2, 'Reaction', 'this is no json', 'broken', ?3, ?4)",
                (&conv_key, NEW, AT, AT + 3_600_000),
            )
            .expect("a parked row that cannot apply");
        answered(&mut h, &attempt, NEW);
        assert_eq!(row_of(&h, NEW).0, attempt.message, "re-keyed");
        assert_eq!(aliases(&h, attempt.message), [OLD]);
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM outbox"),
            0,
            "the send counts"
        );
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM pending_modifiers"),
            0,
            "the parked row is dropped"
        );
    }

    /// The answer comes twice for a row that stands under the server's id with NO position yet (the
    /// first answer stated none in range, R2-L7): the second states it and the row takes it, once; a
    /// third that states another does not move it. Mutant: the `UPDATE ... net_seq = ?2 WHERE ...
    /// net_seq IS NULL` of the "answered twice" branch of `rekey_own` removed.
    #[test]
    fn a_second_answer_gives_a_row_under_the_servers_id_the_position_it_lacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = MsgIdScope::Conversation;
        let mut h = harness(dir.path(), scope);
        let attempt = in_flight(&mut h, OLD);
        let seq = |h: &IngestHarness| {
            value::<Option<i64>>(h.connection(), "SELECT net_seq FROM messages")
        };
        answered_at(&mut h, &attempt, NEW, -1);
        assert_eq!(row_of(&h, NEW).0, attempt.message, "the id is taken");
        assert_eq!(seq(&h), None, "without a position");

        assert!(
            rekey_at(&mut h, scope, attempt.message, NEW, 1_760_000_000_123),
            "the second answer is the same message"
        );
        assert_eq!(seq(&h), Some(1_760_000_000_123), "and teaches the position");
        assert!(rekey_at(
            &mut h,
            scope,
            attempt.message,
            NEW,
            1_760_000_000_999
        ));
        assert_eq!(seq(&h), Some(1_760_000_000_123), "the first one stands");
        assert_eq!(aliases(&h, attempt.message), [OLD]);
        assert_eq!(
            value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
            1
        );
    }

    /// A bad id in the answer (empty, a control character) arrives through `record_outcome`
    /// (R2-L7; `an_id_that_cannot_be_taken_moves_nothing_and_is_no_error` goes straight to
    /// `rekey_own`): the send counts, the outbox row is gone, and the row keeps its allocated id, with
    /// no alias and no position. Mutant: the `validate_local` check of `rekey_own` removed (the id is
    /// written).
    #[test]
    fn a_bad_id_in_the_answer_keeps_the_allocated_one_and_the_send_counts() {
        for bad in ["", "a\u{7}b"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut h = harness(dir.path(), MsgIdScope::Conversation);
            let attempt = in_flight(&mut h, OLD);
            answered(&mut h, &attempt, bad);
            assert_eq!(
                row_of(&h, OLD).0,
                attempt.message,
                "{bad:?}: it keeps its id"
            );
            assert!(aliases(&h, attempt.message).is_empty(), "{bad:?}");
            assert_eq!(
                value::<Option<i64>>(h.connection(), "SELECT net_seq FROM messages"),
                None,
                "{bad:?}"
            );
            assert_eq!(
                value::<i64>(h.connection(), "SELECT count(*) FROM outbox"),
                0,
                "{bad:?}: the send counts"
            );
            let floor: String = value(h.connection(), "SELECT out_floor FROM messages");
            assert!(floor.starts_with("{\"Sent\""), "{bad:?}: {floor}");
            assert_eq!(
                value::<i64>(h.connection(), "SELECT count(*) FROM messages"),
                1,
                "{bad:?}"
            );
        }
    }

    /// A stopped attempt (`CANCEL_STOPPING`: the backend said nothing of it went out) that reports a
    /// send anyway, here `SentAs`, is recorded as sent and never taken back (R2-M4): the message
    /// stays, re-keyed, and its outbox row is gone. One that ends `Failed` is taken back, which is
    /// what the mark promises. Mutant: `| TransmitOutcome::SentAs { .. }` removed from `went` in
    /// `record_outcome`.
    #[test]
    fn a_stopped_attempt_that_reports_a_send_is_recorded_as_sent_and_not_taken_back() {
        for reports_a_send in [true, false] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut h = harness(dir.path(), MsgIdScope::Conversation);
            let attempt = in_flight(&mut h, OLD);
            h.connection()
                .execute(
                    "UPDATE outbox SET cancel_state = ?2 WHERE id = ?1",
                    (attempt.outbox, outbox::CANCEL_STOPPING),
                )
                .expect("the stop was accepted");
            if reports_a_send {
                answered(&mut h, &attempt, NEW);
            } else {
                record(
                    &mut h,
                    &attempt,
                    TransmitOutcome::Failed(kourier_model::BackendError::new(
                        kourier_model::ErrorKind::Transient { retry_after: None },
                        "stopped",
                    )),
                );
            }
            let count = |h: &IngestHarness, sql: &str| value::<i64>(h.connection(), sql);
            assert_eq!(
                count(&h, "SELECT count(*) FROM outbox"),
                0,
                "{reports_a_send}"
            );
            if reports_a_send {
                assert_eq!(row_of(&h, NEW).0, attempt.message, "it stays, re-keyed");
                assert_eq!(aliases(&h, attempt.message), [OLD]);
                let floor: String = value(h.connection(), "SELECT out_floor FROM messages");
                assert!(floor.starts_with("{\"Sent\""), "{floor}");
            } else {
                assert_eq!(
                    count(&h, "SELECT count(*) FROM messages"),
                    0,
                    "the message the user stopped is taken back"
                );
            }
        }
    }

    /// The first sign that somebody has an own message starts its `AfterSend` timer (once), also
    /// when it is the ANSWER that says so and the echo has not come (R2-M4): `SentAs` reaches, a
    /// failure does not. Mutant: `SentAs` answered `false` in `reached` in `record_outcome`.
    #[test]
    fn an_answer_that_names_the_id_starts_the_disappearing_timer_of_a_send() {
        let hour = std::time::Duration::from_secs(3600);
        for reaches in [true, false] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut h = harness(dir.path(), MsgIdScope::Conversation);
            let mut m = own(&chat(), OLD, AT, "hello");
            m.initial_out_state = Some(OutgoingState::Queued);
            m.expiry = Some(Expiry {
                kind: DisappearKind::AfterSend,
                timer: hour,
                started_at: None,
            });
            let attempt = in_flight_with(
                &mut h,
                BackendEvent::Message(m),
                OLD,
                5,
                outbox::TRANSMITTING,
            );
            let timer = |h: &IngestHarness| {
                h.connection()
                    .query_row(
                        "SELECT expiry_started_at, expires_at FROM messages",
                        [],
                        |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
                    )
                    .expect("the row")
            };
            assert_eq!(
                timer(&h),
                (None, None),
                "queued: the timer waits for the send"
            );
            if reaches {
                answered(&mut h, &attempt, NEW);
                assert_eq!(
                    timer(&h),
                    (Some(AT), Some(AT + 3_600_000)),
                    "the answer started it"
                );
            } else {
                record(
                    &mut h,
                    &attempt,
                    TransmitOutcome::Failed(kourier_model::BackendError::new(
                        kourier_model::ErrorKind::Transient { retry_after: None },
                        "failed",
                    )),
                );
                assert_eq!(timer(&h), (None, None), "a failure starts nothing");
            }
        }
    }
}
