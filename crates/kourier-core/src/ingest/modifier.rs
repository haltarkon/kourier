//! Modifiers (§4.9 row 3): edits, deletes, reactions, receipts, read markers, own send states
//! and deletes of one attachment. Each resolves its target through the current and revision
//! keys (§3.1, §3.5.3); a missing target parks the modifier in `pending_modifiers` until the
//! message arrives. A clear of a conversation waits there too, for the anchors it names that
//! are not here yet ([`super::clear`]).
//!
//! A receipt counts only from someone its message went to ([`Apply::receipt_rule`]); from
//! anyone else it waits like a modifier without a target, because the sender may turn out to
//! be a recipient under another id (`IdentityMerged`), and is dropped when its time is up.
//!
//! A modifier that names a conversation is about a message of that conversation and of no
//! other, whatever the network's id scope: an edit, delete or reaction sent in one chat never
//! lands on a message of another chat, however exactly it names author and id. It waits like a
//! modifier whose target is not here (the target may still come into that conversation: two
//! direct chats of one person merge) and is dropped when its time is up.

use std::sync::Arc;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{OptionalExtension, params};
use kourier_model::{
    Body, ConvKey, ConversationFeatures, MemberRole, MessageId, MsgIdScope, MsgTarget, ReadState,
    ReceiptKind, RecipientState, TargetAuthor, Text, UserId,
};

use super::message::{MsgRow, recipient_rank};
use super::{Apply, hex64};
use crate::error::Result;
use crate::sql;

/// How long a receipt, a read marker or a delete for me of another own device (one
/// attachment, a conversation's history) waits for its target: Signal Desktop keeps such sync
/// tasks for two days (`SD/ts/sql/Server.node.ts`: tasks with `createdAt` older than
/// `durations.DAY * 2` are removed).
pub(crate) const PENDING_SHORT_MS: i64 = 2 * 24 * 3600 * 1000;
/// How long other modifiers wait.
const PENDING_LONG_MS: i64 = 7 * 24 * 3600 * 1000;
/// Edit chains (an edit of an edit that waited) are followed this deep.
const MAX_CHAIN: u8 = 16;
/// Longest emoji (bytes) a reaction may carry, sent or received. No emoji comes near it (the
/// longest sequences Unicode recommends, two people with skin tones joined by a heart, are 35
/// bytes), so nothing a client sends as a reaction is cut off; it keeps a peer from storing an
/// arbitrarily long text in every reaction row and showing it in the reaction chips.
pub(crate) const MAX_EMOJI_BYTES: usize = 64;

/// A target with canonical conversation key and author column.
#[derive(Clone, Debug)]
pub(crate) struct CanonTarget {
    /// Canonical conversation key, when the reference names one.
    pub conv: Option<String>,
    /// `None` = unknown author; `Some("")` = own; else the canonical user id.
    pub author: Option<String>,
    /// Local part and alternates.
    pub locals: Vec<Arc<str>>,
}

/// The target of a modifier event (receipts are split per target before this).
fn target_of(e: &BackendEvent) -> Option<&MsgTarget> {
    match e {
        BackendEvent::Edited { target, .. }
        | BackendEvent::Deleted { target, .. }
        | BackendEvent::Reaction { target, .. }
        | BackendEvent::AttachmentDeleted { target, .. }
        | BackendEvent::OwnSendState { target, .. } => Some(target),
        BackendEvent::ReadMarker { up_to, .. } => Some(up_to),
        BackendEvent::Receipts { targets, .. } => targets.first(),
        _ => None,
    }
}

/// The age (whole seconds) a receive window sees for a message dated `from_ms` when its edit
/// or delete is dated `to_ms`: the distance between the two, whichever is later, as Signal
/// Desktop measures it (`SD/ts/util/getMessageAge.std.ts`: `Math.abs(now - timestamp)`), so
/// every client of a chat decides alike.
fn age_secs(from_ms: i64, to_ms: i64) -> u64 {
    to_ms.saturating_sub(from_ms).unsigned_abs() / 1000
}

/// What to do with a receipt for a message that is here ([`Apply::receipt_rule`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiptRule {
    /// It counts.
    Apply,
    /// It never will: the message is not ours.
    Ignore,
    /// Its sender is not known as a recipient of the message (yet).
    Wait,
}

thread_local! {
    /// How many parked receipts this thread decoded to look at them again
    /// ([`Apply::retry_parked_receipts`]): a probe for the test that a transaction of 300
    /// merges looks at each parked receipt once (`testing::receipt_retry_decodes`).
    static RECEIPT_DECODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The count of [`RECEIPT_DECODES`] on this thread.
pub(crate) fn receipt_retry_decodes() -> u64 {
    RECEIPT_DECODES.with(std::cell::Cell::get)
}

/// One parked modifier (`pending_modifiers`).
struct Parked {
    id: i64,
    conv: Option<String>,
    author: Option<String>,
    local: String,
    kind: String,
    payload: String,
}

impl Parked {
    const SELECT: &'static str =
        "SELECT id, conv_key, target_author, target_local, kind, payload FROM pending_modifiers";

    fn from_row(r: &kourier_db::rusqlite::Row<'_>) -> kourier_db::rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            conv: r.get(1)?,
            author: r.get(2)?,
            local: r.get(3)?,
            kind: r.get(4)?,
            payload: r.get(5)?,
        })
    }
}

impl Apply<'_> {
    pub(crate) fn canon_target(&self, t: &MsgTarget) -> Result<CanonTarget> {
        let conv = t
            .conv
            .as_ref()
            .map(|k| self.canon_conv(k).map(|k| k.encode()))
            .transpose()?;
        let author = match &t.author {
            TargetAuthor::Own => Some(String::new()),
            TargetAuthor::User(u) => Some(self.author_col(Some(u))?),
            TargetAuthor::Unknown => None,
        };
        let mut locals = vec![t.local.clone()];
        locals.extend(t.alternates.iter().cloned());
        Ok(CanonTarget {
            conv,
            author,
            locals,
        })
    }

    /// Finds the message a reference points at (§4.9): account-wide by (author, local) for
    /// `AccountAuthor` networks, else within the conversation by local (authors compared when
    /// both are known); revision keys resolve to their message.
    ///
    /// A call line is keyed by its conversation on every network ([`super::call`]): only a
    /// reference that names its conversation finds it.
    ///
    /// A reference that names a conversation only ever resolves to a message of that
    /// conversation. Under `AccountAuthor` scope the key alone would find the message in any
    /// chat, so a member of one chat (or somebody who left a group) could edit, delete or
    /// react to a message of another by naming its author and id: the conversation the
    /// modifier arrived in is compared with the row's. Signal Desktop checks the same thing
    /// from the other side (the sender must belong to the target's conversation,
    /// `SD/ts/messageModifiers/Reactions.preload.ts` `isMessageAMatchForReaction`); this rule
    /// is the stricter one, and no official client sends a modifier outside its target's
    /// chat. `None` as the conversation (receipts, read and viewed syncs, which Signal sends
    /// without one) stays an account-wide lookup.
    pub(crate) fn resolve_target(&self, t: &CanonTarget) -> Result<Option<MsgRow>> {
        if let (MsgIdScope::AccountAuthor, Some(author)) = (self.scope, &t.author) {
            for local in &t.locals {
                let id: Option<i64> = self
                    .conn
                    .prepare_cached(
                        "SELECT id FROM messages WHERE account_id = ?1 AND net_author = ?2
                           AND net_local = ?3 AND net_scope = 0
                         UNION ALL
                         SELECT message_id FROM message_revisions WHERE account_id = ?1
                           AND net_local = ?3 AND net_author = ?2
                         LIMIT 1",
                    )?
                    .query_row((self.acc.id.0, author, local.as_ref()), |r| r.get(0))
                    .optional()?;
                if let Some(id) = id
                    && let Some(row) = self.msg_row(MessageId(id))?
                    && t.conv.as_ref().is_none_or(|c| *c == row.conv_key)
                {
                    return Ok(Some(row));
                }
            }
            // A call line is keyed by its conversation on every network: an own reference
            // that names a conversation may be to the line of that conversation. One that
            // names none never finds a line.
            if author.is_empty()
                && let Some(k) = &t.conv
                && let Some(conv) =
                    sql::conversation_id(self.conn, self.acc.id, &sql::conv_key(k)?)?
            {
                for local in &t.locals {
                    if let Some(row) = self.call_row(conv, local)? {
                        return Ok(Some(row));
                    }
                }
            }
            return Ok(None);
        }
        let conv_id = match &t.conv {
            Some(k) => match sql::conversation_id(self.conn, self.acc.id, &sql::conv_key(k)?)? {
                Some(id) => Some(id.0),
                None => return Ok(None),
            },
            None => None,
        };
        for local in &t.locals {
            let candidates: Vec<(i64, String)> = self
                .conn
                .prepare_cached(
                    "SELECT id, net_author FROM messages WHERE account_id = ?1 AND net_local = ?2
                       AND (?3 IS NULL OR conversation_id = ?3)
                     UNION
                     SELECT r.message_id, r.net_author FROM message_revisions r
                       JOIN messages m ON m.id = r.message_id
                     WHERE r.account_id = ?1 AND r.net_local = ?2
                       AND (?3 IS NULL OR m.conversation_id = ?3)",
                )?
                .query_map((self.acc.id.0, local.as_ref(), conv_id), |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            let matching: Vec<i64> = candidates
                .into_iter()
                .filter(|(_, a)| t.author.as_ref().is_none_or(|ta| ta == a))
                .map(|(id, _)| id)
                .collect();
            // Account-wide lookups by local alone must be unambiguous.
            let unique = conv_id.is_some() || matching.len() == 1;
            if let (Some(id), true) = (matching.first(), unique) {
                let row = self.msg_row(MessageId(*id))?;
                // A call line is found by its conversation and its id, never account-wide.
                if conv_id.is_none()
                    && let Some(r) = &row
                    && r.is_own()
                    && r.body_kind == "System"
                    && super::is_call_line(self.conn, r.id)?
                {
                    continue;
                }
                return Ok(row);
            }
        }
        Ok(None)
    }

    /// Applies a modifier to its target, or parks it.
    pub(crate) fn modifier(&mut self, e: BackendEvent) -> Result<()> {
        // OwnSendState may name our row directly.
        if let BackendEvent::OwnSendState {
            local: Some(local), ..
        } = &e
        {
            let own: Option<i64> = self
                .conn
                .prepare_cached(
                    "SELECT id FROM messages WHERE id = ?1 AND account_id = ?2 AND net_author = ''",
                )?
                .query_row((local.0, self.acc.id.0), |r| r.get(0))
                .optional()?;
            if let Some(id) = own
                && let Some(row) = self.msg_row(MessageId(id))?
            {
                return self.apply_modifier(&row, e, 0);
            }
        }
        let Some(target) = target_of(&e) else {
            return Ok(());
        };
        let t = self.canon_target(target)?;
        match self.resolve_target(&t)? {
            Some(row) => self.apply_modifier(&row, e, 0),
            None => self.park(&t, &e),
        }
    }

    /// Stores a modifier whose target is unknown (deduplicated, so a replay is a no-op).
    fn park(&mut self, t: &CanonTarget, e: &BackendEvent) -> Result<()> {
        let kind = e.name();
        let ttl = match e {
            BackendEvent::Receipts { .. }
            | BackendEvent::ReadMarker { .. }
            | BackendEvent::AttachmentDeleted { .. } => PENDING_SHORT_MS,
            _ => PENDING_LONG_MS,
        };
        let payload = sql::to_json(e)?;
        self.park_raw(t, kind, &payload, &payload, ttl)
    }

    /// Inserts one `pending_modifiers` row for the target `t`. `identity` is what makes two
    /// parked rows of one target and kind the same one (a replay is then a no-op).
    pub(super) fn park_raw(
        &mut self,
        t: &CanonTarget,
        kind: &str,
        payload: &str,
        identity: &str,
        ttl: i64,
    ) -> Result<()> {
        let local = t.locals.first().map_or("", |l| l.as_ref());
        let dedup = hex64(kourier_model::stable_hash(
            format!(
                "{}\u{0}{}\u{0}{local}\u{0}{kind}\u{0}{identity}",
                t.conv.as_deref().unwrap_or("-"),
                t.author.as_deref().unwrap_or("?"),
            )
            .as_bytes(),
        ));
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO pending_modifiers(account_id, conv_key, target_author,
                   target_local, kind, payload, dedup, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                self.acc.id.0,
                t.conv,
                t.author,
                local,
                kind,
                payload,
                dedup,
                self.now,
                self.now.saturating_add(ttl),
            ])?;
        Ok(())
    }

    /// Applies, in arrival order, the parked modifiers whose target is `row` under `local`.
    pub(crate) fn apply_pending(&mut self, row: &MsgRow, local: &str) -> Result<()> {
        self.apply_pending_depth(row.id, local, 0)
    }

    fn apply_pending_depth(&mut self, id: MessageId, local: &str, depth: u8) -> Result<()> {
        if depth > MAX_CHAIN {
            return Ok(());
        }
        let Some(row) = self.msg_row(id)? else {
            return Ok(());
        };
        let sql = format!(
            "{} WHERE account_id = ?1 AND target_local = ?2 ORDER BY id",
            Parked::SELECT
        );
        let parked: Vec<Parked> = self
            .conn
            .prepare_cached(&sql)?
            .query_map((self.acc.id.0, local), Parked::from_row)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        // A call line is found by its conversation and its id, never account-wide: only a
        // modifier that names this conversation waited for it.
        let call_line =
            row.is_own() && row.body_kind == "System" && super::is_call_line(self.conn, row.id)?;
        for p in parked {
            // Same rules as `resolve_target`: a reference that names a conversation is about
            // a message of that conversation only, also under AccountAuthor scope. One that
            // waited for a message of another chat keeps waiting until it expires.
            let conv_ok = match &p.conv {
                Some(c) => *c == row.conv_key,
                None => !call_line,
            };
            let author_ok = p.author.as_ref().is_none_or(|a| *a == row.net_author);
            // Account-wide references are unambiguous only under AccountAuthor scope.
            let scope_ok = p.conv.is_some() || p.author.is_some();
            if !(conv_ok && author_ok && scope_ok) {
                continue;
            }
            self.conn
                .prepare_cached("DELETE FROM pending_modifiers WHERE id = ?1")?
                .execute([p.id])?;
            // Earlier modifiers may have changed the row, or removed it.
            let Some(fresh) = self.msg_row(id)? else {
                continue;
            };
            if p.kind == super::clear::CLEARED_KIND {
                self.late_clear(&fresh, &p.payload)?;
                continue;
            }
            let e: BackendEvent = sql::from_json(&p.payload)?;
            self.apply_modifier(&fresh, e, depth)?;
        }
        Ok(())
    }

    /// Looks again at the modifiers that wait for a message of the conversation `conv_key`
    /// (canonical, encoded) and applies, in arrival order, those whose target is there now.
    /// A merge of two direct chats of one person calls it: a modifier sent in one of them
    /// may name a message that was in the other until then.
    pub(crate) fn retry_pending(&mut self, conv_key: &str) -> Result<()> {
        let sql = format!(
            "{} WHERE account_id = ?1 AND conv_key = ?2 ORDER BY id",
            Parked::SELECT
        );
        let parked: Vec<Parked> = self
            .conn
            .prepare_cached(&sql)?
            .query_map((self.acc.id.0, conv_key), Parked::from_row)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for p in parked {
            if p.kind == super::clear::CLEARED_KIND {
                // A clear waits under the key of its anchor.
                let t = CanonTarget {
                    conv: p.conv.clone(),
                    author: p.author.clone(),
                    locals: vec![p.local.as_str().into()],
                };
                let Some(row) = self.resolve_target(&t)? else {
                    continue;
                };
                self.conn
                    .prepare_cached("DELETE FROM pending_modifiers WHERE id = ?1")?
                    .execute([p.id])?;
                self.late_clear(&row, &p.payload)?;
                continue;
            }
            let e: BackendEvent = sql::from_json(&p.payload)?;
            let Some(target) = target_of(&e) else {
                continue;
            };
            let t = self.canon_target(target)?;
            let Some(row) = self.resolve_target(&t)? else {
                continue;
            };
            self.conn
                .prepare_cached("DELETE FROM pending_modifiers WHERE id = ?1")?
                .execute([p.id])?;
            self.apply_modifier(&row, e, 0)?;
        }
        Ok(())
    }

    /// Looks again at the receipts that wait: after an identity merge the sender of one may
    /// be a recipient of its message (the receipt came from a contact's ACI while the message
    /// went to the chat under their PNI). One whose message is here and whose sender now
    /// counts is applied; the others keep waiting. Runs once per transaction, from
    /// [`Apply::finish`], however many merges it held (`Changes::receipts_to_retry`).
    pub(crate) fn retry_receipts(&mut self) -> Result<()> {
        let sql = format!(
            "{} WHERE account_id = ?1 AND kind = 'Receipts' ORDER BY id",
            Parked::SELECT
        );
        let parked: Vec<Parked> = self
            .conn
            .prepare_cached(&sql)?
            .query_map([self.acc.id.0], Parked::from_row)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        self.retry_parked_receipts(parked)
    }

    /// [`retry_receipts`](Self::retry_receipts) for the receipts that wait for `message`
    /// (under its key or the key of one of its revisions): its send was answered, or another
    /// device reported how it went, so `send_state` may name the sender of one now. A
    /// receipt can overtake the answer of its own transmit to a recipient the conversation's
    /// member list does not have yet.
    pub(crate) fn retry_receipts_of(&mut self, message: MessageId) -> Result<()> {
        let sql = format!(
            "{} WHERE account_id = ?1 AND kind = 'Receipts' AND (
               target_local = (SELECT net_local FROM messages WHERE id = ?2)
               OR target_local IN
                 (SELECT net_local FROM message_revisions WHERE message_id = ?2))
             ORDER BY id",
            Parked::SELECT
        );
        let parked: Vec<Parked> = self
            .conn
            .prepare_cached(&sql)?
            .query_map((self.acc.id.0, message.0), Parked::from_row)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        self.retry_parked_receipts(parked)
    }

    /// [`retry_receipts`](Self::retry_receipts) for the receipts that wait for an own
    /// message of `conv`: its member list gained someone, who may be the sender of one (a
    /// receipt from a member the group's update had not named yet).
    pub(crate) fn retry_receipts_in(&mut self, conv: kourier_model::ConversationId) -> Result<()> {
        let sql = format!(
            "{} WHERE account_id = ?1 AND kind = 'Receipts' AND (
               EXISTS(SELECT 1 FROM messages m WHERE m.account_id = ?1
                 AND m.net_local = target_local AND m.conversation_id = ?2
                 AND m.net_author = '')
               OR EXISTS(SELECT 1 FROM message_revisions v
                 JOIN messages m ON m.id = v.message_id WHERE v.account_id = ?1
                 AND v.net_local = target_local AND m.conversation_id = ?2
                 AND m.net_author = ''))
             ORDER BY id",
            Parked::SELECT
        );
        let parked: Vec<Parked> = self
            .conn
            .prepare_cached(&sql)?
            .query_map((self.acc.id.0, conv.0), Parked::from_row)?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        self.retry_parked_receipts(parked)
    }

    /// Applies those of the parked receipts `parked` whose message is here and whose sender
    /// counts now ([`Self::receipt_rule`]); the others keep waiting, with the time they have
    /// left.
    fn retry_parked_receipts(&mut self, parked: Vec<Parked>) -> Result<()> {
        for p in parked {
            RECEIPT_DECODES.with(|n| n.set(n.get() + 1));
            let e: BackendEvent = sql::from_json(&p.payload)?;
            let BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            } = e
            else {
                continue;
            };
            let Some(target) = targets.first() else {
                continue;
            };
            let t = self.canon_target(target)?;
            let Some(row) = self.resolve_target(&t)? else {
                continue;
            };
            let by_col = self.author_col(Some(&by))?;
            if self.receipt_rule(&row, &by_col)? != ReceiptRule::Apply {
                continue;
            }
            self.conn
                .prepare_cached("DELETE FROM pending_modifiers WHERE id = ?1")?
                .execute([p.id])?;
            self.receipt(&row, &by, kind, at)?;
        }
        Ok(())
    }

    fn apply_modifier(&mut self, row: &MsgRow, e: BackendEvent, depth: u8) -> Result<()> {
        // A call line is no message we sent, although it has no author: it takes no receipt,
        // no send state, no reaction and no edit. Only our own devices' delete of the call
        // entry and their read marker apply to it.
        if row.is_own() && row.body_kind == "System" && super::is_call_line(self.conn, row.id)? {
            return match e {
                BackendEvent::Deleted {
                    by,
                    for_everyone: false,
                    at,
                    ..
                } => self.delete(row, by.as_ref(), false, at),
                BackendEvent::ReadMarker { .. } => self.read_marker(row),
                _ => Ok(()),
            };
        }
        // A receipt from someone the message did not go to waits: it has no effect unless
        // its sender turns out to be a recipient.
        if let BackendEvent::Receipts { targets, by, .. } = &e {
            let by_col = self.author_col(Some(by))?;
            match self.receipt_rule(row, &by_col)? {
                ReceiptRule::Apply => {}
                ReceiptRule::Ignore => return Ok(()),
                ReceiptRule::Wait => {
                    let Some(target) = targets.first() else {
                        return Ok(());
                    };
                    let t = self.canon_target(target)?;
                    return self.park(&t, &e);
                }
            }
        }
        match e {
            BackendEvent::Edited {
                by,
                new_local,
                body,
                edited_at,
                keeps_quote,
                ..
            } => {
                let edit = Edit {
                    new_local,
                    body,
                    edited_at,
                    drops_quote: keeps_quote == Some(false),
                };
                self.edit(row, by.as_ref(), edit, depth)
            }
            BackendEvent::Deleted {
                by,
                for_everyone,
                at,
                ..
            } => self.delete(row, by.as_ref(), for_everyone, at),
            BackendEvent::Reaction {
                by,
                emoji,
                remove,
                at,
                ..
            } => self.reaction(row, by.as_ref(), &emoji, remove, at),
            BackendEvent::Receipts { by, kind, at, .. } => self.receipt(row, &by, kind, at),
            BackendEvent::AttachmentDeleted { net_ids, .. } => {
                self.attachment_deleted(row, net_ids.expose())
            }
            BackendEvent::ReadMarker { .. } => self.read_marker(row),
            BackendEvent::OwnSendState { per_recipient, .. } => {
                self.send_state(row, &per_recipient)
            }
            _ => Ok(()),
        }
    }

    /// The conversation's reported features; `None` until the backend sends them (then the
    /// receive windows are not checked, only authorship).
    pub(crate) fn features(&self, row: &MsgRow) -> Result<Option<ConversationFeatures>> {
        let f: Option<String> = self
            .conn
            .prepare_cached("SELECT features FROM conversations WHERE id = ?1")?
            .query_row([row.conv.0], |r| r.get(0))?;
        sql::from_json_opt(f)
    }

    /// Whether `row` is in the chat with ourselves (Signal's Note to Self).
    pub(crate) fn note_to_self(&self, row: &MsgRow) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT is_self FROM conversations WHERE id = ?1")?
            .query_row([row.conv.0], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    /// An edit: only from the author, within `edit_receive` (§3.8), measured from the
    /// original's server time to the edit's ([`MsgRow::dated_at`]); Note to Self has no window
    /// and no count, as in Signal Desktop (`SD/ts/util/handleEditMessage.preload.ts`: the 48 h
    /// check is skipped for `isMessageNoteToSelf`). Only a text or a caption is ever edited
    /// ([`edited_body`]): an edit of anything else changes nothing. An edit of a deleted message
    /// keeps its revision key (so later references resolve) without a body. An edit of a message
    /// with attachments is its new caption: the attachments stay (rows, files and their order),
    /// whatever the edit's body lists; an empty text removes the caption (§3.5.3).
    fn edit(&mut self, row: &MsgRow, by: Option<&UserId>, edit: Edit, depth: u8) -> Result<()> {
        let Edit {
            new_local,
            mut body,
            edited_at,
            drops_quote,
        } = edit;
        if self.author_col(by)? != row.net_author {
            return Ok(());
        }
        let features = self.features(row)?;
        if row.deleted.is_none()
            && let Some(f) = &features
        {
            match f.edit_receive {
                None => return Ok(()),
                Some(_) if self.note_to_self(row)? => {}
                Some(w) if !w.allows(age_secs(row.dated_at(), edited_at), row.revision) => {
                    return Ok(());
                }
                Some(_) => {}
            }
        }
        body.sanitize_ranges();
        if row.deleted.is_none() {
            let (current, view_once): (String, bool) = self
                .conn
                .prepare_cached("SELECT body, view_once FROM messages WHERE id = ?1")?
                .query_row([row.id.0], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if view_once {
                // Its content is shown once and then gone: there is nothing to edit.
                return Ok(());
            }
            match edited_body(sql::from_json(&current)?, body, features.as_ref()) {
                Some(merged) => body = merged,
                // Not a text or a caption this message can take: it changes nothing.
                None => return Ok(()),
            }
        }
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_revisions(message_id, account_id, net_author,
                   net_local, revision, body, edited_at)
                 SELECT id, account_id, net_author, net_local, 0,
                   CASE WHEN deleted IS NULL THEN body END, NULL
                 FROM messages WHERE id = ?1",
            )?
            .execute([row.id.0])?;
        let local = new_local
            .clone()
            .map_or_else(|| format!("~{edited_at}"), |l| l.to_string());
        let stored_body = if row.deleted.is_none() {
            Some(sql::to_json(&body)?)
        } else {
            None
        };
        let inserted = self
            .conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_revisions(message_id, account_id, net_author,
                   net_local, revision, body, edited_at) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            )?
            .execute(params![
                row.id.0,
                self.acc.id.0,
                row.net_author,
                local,
                stored_body,
                edited_at
            ])?;
        if inserted == 0 {
            return Ok(());
        }
        if drops_quote {
            // An edit without a quote takes the quote off the message (Signal Desktop
            // `handleEditMessage`: "quote can be removed but not modified"). Whichever
            // edit says so, in whatever order they arrive: once gone, it stays gone.
            self.conn
                .prepare_cached(
                    "UPDATE messages SET reply_to = NULL WHERE id = ?1 AND reply_to IS NOT NULL",
                )?
                .execute([row.id.0])?;
        }
        self.reindex_revisions(row.id)?;
        self.ch.message(row.conv, row.id);
        if let Some(nl) = new_local {
            self.apply_pending_depth(row.id, &nl, depth + 1)?;
        }
        Ok(())
    }

    /// A delete: for everyone only by the author (or an admin with `admin_delete`) within
    /// `delete_receive`, measured from the message's server time to the delete's
    /// ([`MsgRow::dated_at`]); for me only from our own devices. The admin may be our own
    /// account: a delete for everyone of another member's message that our phone synced
    /// (`by: None`) counts when one of our ids is an admin of the conversation, as Signal
    /// Desktop checks it for its own account (`SD/ts/util/canDeleteForEveryone.preload.ts`
    /// `canReceiveDeleteForEveryone`: `isMemberGroupAdmin(targetConversation,
    /// deleteSentByAci)`). A tombstone keeps the row and its keys (§3.5.2); delete-for-me wins
    /// over delete-for-everyone.
    fn delete(
        &mut self,
        row: &MsgRow,
        by: Option<&UserId>,
        for_everyone: bool,
        at: i64,
    ) -> Result<()> {
        let by_col = self.author_col(by)?;
        let kind = if for_everyone {
            let features = self.features(row)?;
            let by_admin = by_col != row.net_author
                && features.as_ref().is_some_and(|f| f.admin_delete)
                && if by_col.is_empty() {
                    self.own_admin(row)?
                } else {
                    self.is_admin(row, &by_col)?
                };
            if by_col != row.net_author && !by_admin {
                return Ok(());
            }
            if let Some(f) = &features {
                match f.delete_receive {
                    None => return Ok(()),
                    Some(w) if !w.allows(age_secs(row.dated_at(), at), 0) => return Ok(()),
                    Some(_) => {}
                }
            }
            "ForEveryone"
        } else {
            if !by_col.is_empty() {
                return Ok(());
            }
            "ForMe"
        };
        match row.deleted.as_deref() {
            Some("ForMe") => return Ok(()),
            Some("ForEveryone") if kind == "ForEveryone" => return Ok(()),
            _ => {}
        }
        self.tombstone(row, kind, &by_col)
    }

    /// Turns `row` into a tombstone (§3.5.2): `kind` is `ForEveryone` or `ForMe`, `by_col` the
    /// deleter's author column. The row and its keys stay, so later modifiers still resolve;
    /// the body, revision bodies, quote, raw content and reactions go, the attachments are
    /// deleted with their files (unlinked after the commit), and so are its outbox rows other
    /// than deletes.
    pub(crate) fn tombstone(&mut self, row: &MsgRow, kind: &str, by_col: &str) -> Result<()> {
        let empty = sql::to_json(&Body::Text(Text::default()))?;
        // A call line that is deleted rings no more and has no read state left: what is
        // left of it is the same row whatever the line was when its delete arrived (also its
        // `origin`, which said which way the call went).
        let call_line =
            row.is_own() && row.body_kind == "System" && super::is_call_line(self.conn, row.id)?;
        self.conn
            .prepare_cached(
                "UPDATE messages SET deleted = ?2, deleted_by = ?3, body = ?4, body_kind = 'Text',
                   preview = NULL, reply_to = NULL, raw = NULL, call_deadline = NULL,
                   read_state = CASE WHEN ?5 THEN NULL ELSE read_state END,
                   origin = CASE WHEN ?5 THEN ?6 ELSE origin END
                 WHERE id = ?1",
            )?
            .execute((row.id.0, kind, by_col, empty, call_line, super::DEAD_ORIGIN))?;
        self.conn
            .prepare_cached("UPDATE message_revisions SET body = NULL WHERE message_id = ?1")?
            .execute([row.id.0])?;
        self.conn
            .prepare_cached("DELETE FROM reactions WHERE message_id = ?1")?
            .execute([row.id.0])?;
        self.delete_attachments(row.id)?;
        // Nothing of a deleted message is sent any more: its own send (also a `Failed` row
        // kept for "Retry", which nothing would ever remove), its edits and its reactions. A
        // delete still goes out, and a transmit in flight is answered first
        // (`outbox::record_outcome` drops its row then).
        let dropped = self
            .conn
            .prepare_cached(
                "DELETE FROM outbox WHERE message_id = ?1 AND kind <> 'Delete'
                   AND state <> 'Transmitting'",
            )?
            .execute([row.id.0])?;
        // The rows behind them need not wait for a timer that was armed for one of these.
        self.ch.outbox |= dropped > 0;
        self.ch.message(row.conv, row.id);
        Ok(())
    }

    pub(crate) fn is_admin(&self, row: &MsgRow, user: &str) -> Result<bool> {
        let role: Option<String> = self
            .conn
            .prepare_cached("SELECT role FROM members WHERE conversation_id = ?1 AND user_id = ?2")?
            .query_row((row.conv.0, user), |r| r.get(0))
            .optional()?;
        Ok(role.is_some_and(|r| r == sql::enum_name(&MemberRole::Admin).unwrap_or_default()))
    }

    /// Whether one of our own ids is an admin of `row`'s conversation.
    pub(crate) fn own_admin(&self, row: &MsgRow) -> Result<bool> {
        for own in &self.acc.own_ids {
            if self.is_admin(row, own.as_str())? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A reaction: last writer wins per (user, emoji) by `at`, removals stay as tombstones.
    /// One longer than any emoji ([`MAX_EMOJI_BYTES`]) is not a reaction and is dropped.
    pub(crate) fn reaction(
        &mut self,
        row: &MsgRow,
        by: Option<&UserId>,
        emoji: &str,
        remove: bool,
        at: i64,
    ) -> Result<()> {
        if row.deleted.is_some() || emoji.is_empty() || emoji.len() > MAX_EMOJI_BYTES {
            return Ok(());
        }
        let user = self.author_col(by)?;
        let n = self.merge_reaction(row.id, &user, emoji, at, remove)?;
        if n > 0 {
            self.ch.message(row.conv, row.id);
        }
        Ok(())
    }

    /// LWW upsert of one reaction row; returns the number of rows written.
    pub(crate) fn merge_reaction(
        &self,
        id: MessageId,
        user: &str,
        emoji: &str,
        at: i64,
        removed: bool,
    ) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached(
                "INSERT INTO reactions(message_id, user_id, emoji, at, removed)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(message_id, user_id, emoji) DO UPDATE SET
                   at = excluded.at, removed = excluded.removed
                 WHERE excluded.at > reactions.at
                    OR (excluded.at = reactions.at AND excluded.removed > reactions.removed)",
            )?
            .execute((id.0, user, emoji, at, removed))?)
    }

    /// Earliest-wins upsert of one receipt row; returns the number of rows written.
    pub(crate) fn merge_receipt(
        &self,
        id: MessageId,
        user: &str,
        kind: &str,
        at: i64,
    ) -> Result<usize> {
        Ok(self
            .conn
            .prepare_cached(
                "INSERT INTO receipts(message_id, user_id, kind, at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(message_id, user_id, kind) DO UPDATE SET at = excluded.at
                 WHERE excluded.at < receipts.at",
            )?
            .execute((id.0, user, kind, at))?)
    }

    /// Forward-only upsert of one per-recipient send state; returns whether it changed.
    pub(crate) fn merge_send_state(
        &self,
        id: MessageId,
        user: &str,
        state: &RecipientState,
        error: Option<&str>,
    ) -> Result<bool> {
        let current: Option<String> = self
            .conn
            .prepare_cached("SELECT state FROM send_state WHERE message_id = ?1 AND user_id = ?2")?
            .query_row((id.0, user), |r| r.get(0))
            .optional()?;
        if let Some(c) = current {
            let c: RecipientState = sql::from_json(&c)?;
            if recipient_rank(state) <= recipient_rank(&c) {
                return Ok(false);
            }
        }
        self.conn
            .prepare_cached(
                "INSERT INTO send_state(message_id, user_id, state, error, at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(message_id, user_id) DO UPDATE SET
                   state = excluded.state, error = excluded.error, at = excluded.at",
            )?
            .execute((id.0, user, sql::to_json(state)?, error, self.now))?;
        Ok(true)
    }

    /// Whether a receipt from `by_col` (an author column: `''` = one of our own devices)
    /// counts for `row`. From ourselves always (it is our read of an incoming message, synced
    /// from another device). From someone else only for a message of our own that went to
    /// them: the peer of its direct chat, a member of its group, or someone with a send
    /// state on it (a member who left since, a recipient the backend named). Signal Desktop
    /// drops a receipt whose sender is not among the message's recipients
    /// (`SD/ts/messageModifiers/MessageReceipts.preload.ts` `getTargetMessage`:
    /// `sendStateByConversationId` must have the sender); without the rule anybody who
    /// knows (or guesses) the timestamp of a message could mark it delivered, read or
    /// viewed. A message of somebody else takes no receipt from a third party at all.
    ///
    /// The send state is not asked to be `Sent` (Desktop asks): a receipt can overtake the
    /// answer of its own transmit, and membership already says the message went there.
    fn receipt_rule(&self, row: &MsgRow, by_col: &str) -> Result<ReceiptRule> {
        if by_col.is_empty() {
            return Ok(ReceiptRule::Apply);
        }
        if !row.is_own() {
            return Ok(ReceiptRule::Ignore);
        }
        let sent_to: bool = match sql::conv_key(&row.conv_key)? {
            ConvKey::Direct(peer) => peer.as_str() == by_col,
            _ => self
                .conn
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM members WHERE conversation_id = ?1
                       AND user_id = ?2)",
                )?
                .query_row((row.conv.0, by_col), |r| r.get(0))?,
        };
        let has_state = |conn: &kourier_db::rusqlite::Connection| -> Result<bool> {
            Ok(conn
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM send_state WHERE message_id = ?1
                       AND user_id = ?2)",
                )?
                .query_row((row.id.0, by_col), |r| r.get(0))?)
        };
        Ok(if sent_to || has_state(self.conn)? {
            ReceiptRule::Apply
        } else {
            ReceiptRule::Wait
        })
    }

    /// A receipt that counts ([`Self::receipt_rule`]). From another own device it moves our
    /// read state of an incoming message; otherwise it is stored and folds into the
    /// aggregated outgoing state.
    fn receipt(&mut self, row: &MsgRow, by: &UserId, kind: ReceiptKind, at: i64) -> Result<()> {
        let by_col = self.author_col(Some(by))?;
        if by_col.is_empty() {
            let state = match kind {
                ReceiptKind::Read => ReadState::Read,
                ReceiptKind::Viewed => ReadState::Viewed,
                ReceiptKind::Delivered => return Ok(()),
            };
            if !row.is_own() {
                self.raise_read_state(row, state)?;
            }
            return Ok(());
        }
        let n = self.merge_receipt(row.id, &by_col, &sql::enum_name(&kind)?, at)?;
        if n > 0 {
            self.ch.message(row.conv, row.id);
            if row.is_own() {
                self.recompute_out(row.id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn raise_read_state(&mut self, row: &MsgRow, state: ReadState) -> Result<()> {
        let current: Option<ReadState> = row
            .read_state
            .as_deref()
            .map(sql::from_enum_name)
            .transpose()?;
        if current.is_some_and(|c| c >= state) {
            return Ok(());
        }
        self.conn
            .prepare_cached("UPDATE messages SET read_state = ?2 WHERE id = ?1")?
            .execute((row.id.0, sql::enum_name(&state)?))?;
        self.start_read_timer(row.id)?;
        self.ch.message(row.conv, row.id);
        Ok(())
    }

    /// A read marker from another own device: everything up to the target's `sent_at` is read,
    /// including messages that arrive later with an older `sent_at`.
    fn read_marker(&mut self, row: &MsgRow) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE conversations SET
                   read_sent_at = max(coalesce(read_sent_at, ?2), ?2),
                   read_up_to = max(coalesce(read_up_to, ?3), ?3)
                 WHERE id = ?1",
            )?
            .execute((row.conv.0, row.sent_at, row.sort_key))?;
        let ids: Vec<i64> = self
            .conn
            .prepare_cached(super::READ_UP_TO)?
            .query_map((row.conv.0, row.sent_at), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for id in ids {
            self.start_read_timer(MessageId(id))?;
            self.ch.message(row.conv, MessageId(id));
        }
        self.ch.conversations.insert(row.conv);
        self.ch.recount_conv(row.conv);
        Ok(())
    }

    /// Per-recipient send states of an own message (sync transcripts, late outcomes).
    fn send_state(&mut self, row: &MsgRow, per: &[(UserId, RecipientState)]) -> Result<()> {
        if !row.is_own() {
            return Ok(());
        }
        let mut changed = false;
        for (u, st) in per {
            let uc = self.author_col(Some(u))?;
            if uc.is_empty() {
                continue;
            }
            changed |= self.merge_send_state(row.id, &uc, st, None)?;
        }
        if changed {
            self.ch.message(row.conv, row.id);
            self.recompute_out(row.id)?;
            // Someone has a send state on the message now: a receipt of theirs that
            // arrived first counts.
            self.retry_receipts_of(row.id)?;
        }
        Ok(())
    }
}

/// One incoming edit (`BackendEvent::Edited` without its target and author).
struct Edit {
    /// The `local` of the new revision.
    new_local: Option<Arc<str>>,
    /// The new content.
    body: Body,
    /// When.
    edited_at: i64,
    /// The edit carries no quote: one the message had is removed.
    drops_quote: bool,
}

/// The body an edit leaves behind, given the message's `current` body (§3.5.3), or `None`
/// when the edit does not apply.
///
/// Only the core sees the target, so it decides what can be edited (the backend decides what
/// only it sees: the wire form of the edit). An edit is a new text, never anything else:
///
/// - a text message takes the text of the edit (not an empty one);
/// - a message with attachments keeps them, in order, and takes the text as its caption (none
///   when the text is empty). An edit never adds, removes or replaces a file. Where the chat's
///   features say that one of its file kinds carries no caption (a voice note on Signal), an
///   edit that brings one is dropped: Signal Desktop drops every edit of a voice message
///   (`SD/ts/util/handleEditMessage.preload.ts`), and no client of the chat could have sent
///   it. Unknown features leave only the author check;
/// - a message that could not be decrypted and still waits for its resend takes the text: it
///   is the author's newest wording for that id;
/// - a sticker, a contact card, a location, a poll, a notice of the timeline, an expired or
///   opened view-once stand-in and content Kourier does not understand are never edited
///   (Desktop: contact shares and view-once, same file), and neither is anything by an edit
///   whose own body is not a text or a caption.
fn edited_body(current: Body, edit: Body, features: Option<&ConversationFeatures>) -> Option<Body> {
    let filled = |t: Text| (!t.body.trim().is_empty()).then_some(t);
    let text = match edit {
        Body::Text(t) => filled(t),
        Body::Media { caption, .. } => caption.and_then(filled),
        _ => return None,
    };
    match current {
        Body::Text(_) | Body::Placeholder(kourier_model::Placeholder::Undecryptable { .. }) => {
            text.map(Body::Text)
        }
        Body::Media {
            attachments,
            caption,
        } => {
            let current = Body::Media {
                attachments,
                caption,
            };
            let takes_caption =
                crate::actions::media_of(&current, features).is_some_and(|m| m.captionable);
            if text.is_some() && !takes_caption {
                return None;
            }
            let Body::Media { attachments, .. } = current else {
                return None;
            };
            Some(Body::Media {
                attachments,
                caption: text,
            })
        }
        _ => None,
    }
}

impl super::Changes {
    /// Marks a conversation's counters for recomputation.
    pub(crate) fn recount_conv(&mut self, conv: kourier_model::ConversationId) {
        self.recount.insert(conv);
    }
}
