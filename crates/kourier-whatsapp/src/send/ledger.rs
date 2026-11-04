//! The outcome ledger (W5a, decision D4; brief §2.3 with critic H2 and L1): what the server answered to a
//! message Kourier sent, kept in `backend.db` (`send_outcomes`, migration 5).
//!
//! ORDER. The outcome is committed in ONE zone ([`commit`], kill points `wa_inside_send_outcome` /
//! `wa_after_send_outcome`) BEFORE the core is answered, and every Transmit reads it first ([`look`]): `acked`
//! is answered `Sent{server_at}` and `refused` `Failed(ReachOutRefused)`, both with NO frame. So a resend whose
//! outcome the core lost (a kill after the commit, before the core committed its own) never reaches the server
//! again, and a 463 is never met twice that way.
//!
//! ONLY `acked` AND `refused` (critic H2): a failure is not recorded. The core takes Send Again for every
//! failure but the 463 and resends with the same id; a nacked stanza is one the server does not have, so the
//! resend cannot duplicate anything, and a recorded failure would make Send Again a dead button for 14 days.
//!
//! IN THE SAME ZONE: an acked message into a direct chat is the started fact (`send::started`, How::Acked),
//! and an acked REVOKE takes upstream's `sent_messages` rows of the original AND of every edit of it (critic L1:
//! an edit's row holds the edited text under its own id for up to 7200 s), under every form of the chat
//! upstream may have keyed them by (the chat as its LID when known, else as it is): a late retry receipt
//! then finds nothing to send again of a text the person deleted. The edits are found by what the row HOLDS (a
//! protocol message `MESSAGE_EDIT` whose key is the revoked id), whatever the edit's own ack was, so an edit whose
//! ack was never recorded (lost, or not yet committed) does not keep its text (fix F18); an edit whose ack is
//! recorded AFTER the revoke's is taken in the zone that records it ([`record`]). The same zone deletes the revoked
//! message's UPLOAD RECORD (`send::record`, fix F13): the media key, the paths and the preview of a file the person
//! deleted for everyone are not kept for 30 days.
//!
//! Rows go at a start after [`KEEP_MS`] ([`sweep`]): they are needed only between our answer and the core's
//! commit of its outcome.

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, Transaction, params};
use kourier_model::{ConvKey, MsgTarget, ReceiptKind, TargetAuthor, UserId};
use wacore::store::error::StoreError;

use super::address::jid_of;
use super::started::{self, How};
use crate::db::Points;
use crate::store::{KourierStore, exec, first, rows};

/// The kill points of the outcome zone.
pub(crate) const POINTS: Points = Points {
    inside: crate::failpoints::INSIDE_SEND_OUTCOME,
    after: crate::failpoints::AFTER_SEND_OUTCOME,
};

/// How long a row is kept: 14 days.
pub(crate) const KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// What was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A new message (text; files from W5b).
    Message,
    /// An edit of an own message.
    Edit,
    /// A delete for everyone of an own message.
    Revoke,
    /// A reaction or its removal.
    React,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Edit => "edit",
            Self::Revoke => "revoke",
            Self::React => "react",
        }
    }
}

/// What the server answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The server took it; `server_t` is the ack's time in seconds when it said one.
    Acked { server_t: Option<i64> },
    /// The server refused it with 463 (a reach-out the account may not make).
    Refused,
}

/// One outcome to record.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// The request's conversation.
    pub(crate) conv: ConvKey,
    /// The core's id of the message, which is also its WhatsApp id.
    pub(crate) local: String,
    /// What was sent.
    pub(crate) kind: Kind,
    /// The core's id of the message an edit or a revoke is about.
    pub(crate) target: Option<String>,
    /// The answer.
    pub(crate) outcome: Outcome,
}

/// What a record did besides its own row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Recorded {
    /// The row is new (a second record of one local keeps the first).
    pub(crate) new: bool,
    /// Upstream's `sent_messages` rows an acked revoke took.
    pub(crate) taken: usize,
}

/// The recorded outcome of the message Kourier sent as `local`, if any. By the local ALONE (fix F13): the core's
/// locals are unique per account, and the conversation form a resend asks with may not be the one the row was
/// recorded under (a `pn:` chat the core re-keyed to its `lid:` form between the send and the resend), which a look
/// by `(conv, local)` would miss: the resend would write the frame again, and a 463 would be met twice. The oldest
/// row answers (the first record wins).
pub(crate) fn look(conn: &Connection, local: &str) -> Result<Option<Outcome>, StoreError> {
    first(
        conn,
        "look up a send outcome",
        "SELECT outcome, server_t FROM send_outcomes WHERE local = ?1 ORDER BY at, conv LIMIT 1",
        params![local],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
    )
    .map(|row| {
        row.map(|(outcome, server_t)| match outcome.as_str() {
            "acked" => Outcome::Acked { server_t },
            _ => Outcome::Refused,
        })
    })
}

/// The server's time (seconds) of an acked message Kourier sent, when the ledger still has it: the edit
/// window judges the original's age by it (decision D9). By the local alone, like [`look`].
pub(crate) fn acked_at(conn: &Connection, local: &str) -> Result<Option<i64>, StoreError> {
    look(conn, local).map(|outcome| match outcome {
        Some(Outcome::Acked { server_t }) => server_t,
        _ => None,
    })
}

/// The conversation the ledger holds for the message Kourier sent as `local`, whatever form of the chat the
/// request named (the core's locals are unique per account: one row at most). `None` for a message Kourier did
/// not send, or one whose outcome is not recorded (yet).
pub(crate) fn conv_of(conn: &Connection, local: &str) -> Result<Option<ConvKey>, StoreError> {
    first(
        conn,
        "look up the chat of a sent message",
        "SELECT conv FROM send_outcomes WHERE local = ?1 LIMIT 1",
        params![local],
        |r| r.get::<_, String>(0),
    )
    .map(|conv| conv.and_then(|conv| ConvKey::parse(&conv).ok()))
}

/// Whether `events` hold a receipt of OUR messages in a direct chat: the receipts [`rehome_receipts`] may move.
pub(crate) fn names_an_own_receipt(events: &[BackendEvent]) -> bool {
    events.iter().any(|event| match event {
        BackendEvent::Receipts { targets, .. } => targets.iter().any(is_direct_own),
        _ => false,
    })
}

fn is_direct_own(target: &MsgTarget) -> bool {
    matches!(target.author, TargetAuthor::Own) && matches!(target.conv, Some(ConvKey::Direct(_)))
}

/// `events` with the receipts of OUR messages in a direct chat moved to the conversation the ledger holds for each
/// message (fix F9): Kourier writes a chat the core keys by the NUMBER to its LID (D20), so the peer's receipts come
/// back from the LID, and the core has no conversation under that form (the pair is learned by usync, which states no
/// merge); the ledger knows the `(conv, local)` of every message Kourier sent. A target moves only when the ledger's
/// conversation is a direct chat that is ANOTHER FORM OF THE SAME PERSON by the store's mapping ([`started::forms`]),
/// and its receipt's `by` becomes that conversation's peer (the core applies a receipt to a 1:1 message only from its
/// peer). A message the ledger does not know (one sent from the phone) keeps today's path; targets of one receipt
/// that land in different chats are split into one receipt each. `None` when nothing moves.
pub(crate) fn rehome_receipts(
    conn: &Connection,
    events: &[BackendEvent],
) -> Result<Option<Vec<BackendEvent>>, StoreError> {
    let mut out: Option<Vec<BackendEvent>> = None;
    for (i, event) in events.iter().enumerate() {
        let moved = match event {
            BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            } => rehome(conn, targets, by, *kind, *at)?,
            _ => None,
        };
        match (moved, out.as_mut()) {
            (Some(moved), Some(out)) => out.extend(moved),
            (Some(moved), None) => {
                let mut all = events[..i].to_vec();
                all.extend(moved);
                out = Some(all);
            }
            (None, Some(out)) => out.push(event.clone()),
            (None, None) => {}
        }
    }
    Ok(out)
}

/// One receipt's targets by the chat the ledger holds for them (see [`rehome_receipts`]); `None` when none moves.
fn rehome(
    conn: &Connection,
    targets: &[MsgTarget],
    by: &UserId,
    kind: ReceiptKind,
    at: i64,
) -> Result<Option<Vec<BackendEvent>>, StoreError> {
    let mut groups: Vec<(Option<ConvKey>, UserId, Vec<MsgTarget>)> = Vec::new();
    let mut moved = false;
    for target in targets {
        let mut target = target.clone();
        let mut by = by.clone();
        if is_direct_own(&target)
            && let Some(ConvKey::Direct(peer)) = home_of(conn, &target)?
        {
            target.conv = Some(ConvKey::Direct(peer.clone()));
            by = peer;
            moved = true;
        }
        match groups
            .iter_mut()
            .find(|(conv, who, _)| *conv == target.conv && *who == by)
        {
            Some((_, _, all)) => all.push(target),
            None => groups.push((target.conv.clone(), by, vec![target])),
        }
    }
    Ok(moved.then(|| {
        groups
            .into_iter()
            .map(|(_, by, targets)| BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            })
            .collect()
    }))
}

/// The ledger's conversation for `target`'s message when it is another form of the target's own chat.
fn home_of(conn: &Connection, target: &MsgTarget) -> Result<Option<ConvKey>, StoreError> {
    let Some(stated) = &target.conv else {
        return Ok(None);
    };
    let Some(home @ ConvKey::Direct(_)) = conv_of(conn, &target.local)? else {
        return Ok(None);
    };
    if home == *stated || !started::forms(conn, &home)?.contains(stated) {
        return Ok(None);
    }
    Ok(Some(home))
}

/// Records `entry` in the caller's zone (module docs: the started fact and a revoke's take go with it).
pub(crate) fn record(
    tx: &Transaction<'_>,
    entry: &Entry,
    at_ms: i64,
) -> Result<Recorded, StoreError> {
    let (outcome, server_t) = match entry.outcome {
        Outcome::Acked { server_t } => ("acked", server_t),
        Outcome::Refused => ("refused", None),
    };
    let new = exec(
        tx,
        "record a send outcome",
        "INSERT INTO send_outcomes(conv, local, kind, outcome, target, server_t, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (conv, local) DO NOTHING",
        params![
            entry.conv.encode(),
            entry.local,
            entry.kind.as_str(),
            outcome,
            entry.target,
            server_t,
            at_ms,
        ],
    )? > 0;
    let mut recorded = Recorded { new, taken: 0 };
    if !matches!(entry.outcome, Outcome::Acked { .. }) {
        return Ok(recorded);
    }
    started::note(tx, &entry.conv, How::Acked)?;
    match (entry.kind, &entry.target) {
        (Kind::Revoke, Some(target)) => {
            recorded.taken = take_sent(tx, &entry.conv, target)?;
            // What the person deleted for everyone leaves the backend in this zone, the upload record of a file too:
            // its media key, paths and preview serve no edit or re-upload of a revoked message (fix F13).
            super::record::forget(tx, target)?;
        }
        // A late ack of an edit whose original was revoked meanwhile: its row holds the text the person deleted.
        (Kind::Edit, Some(target)) if revoked(tx, target)? => {
            recorded.taken = take_copies(tx, &entry.conv, std::slice::from_ref(&entry.local))?;
        }
        _ => {}
    }
    Ok(recorded)
}

/// [`record`] in its own zone with the kill points, on the store's writer: `Ok` only after `COMMIT`.
pub(crate) async fn commit(
    store: &KourierStore,
    entry: Entry,
    at_ms: i64,
) -> Result<Recorded, StoreError> {
    store
        .write_at("record a send outcome", POINTS, move |tx| {
            record(tx, &entry, at_ms)
        })
        .await
}

/// Whether an acked revoke of the message `target` is recorded.
fn revoked(tx: &Transaction<'_>, target: &str) -> Result<bool, StoreError> {
    first(
        tx,
        "look up a revoke of a message",
        "SELECT 1 FROM send_outcomes WHERE target = ?1 AND kind = 'revoke' AND outcome = 'acked' LIMIT 1",
        params![target],
        |_| Ok(()),
    )
    .map(|found| found.is_some())
}

/// Takes upstream's `sent_messages` rows of `target` and of every edit of it (whatever the edit's ack), under every
/// form of the chat.
fn take_sent(tx: &Transaction<'_>, conv: &ConvKey, target: &str) -> Result<usize, StoreError> {
    let mut ids = vec![target.to_owned()];
    ids.extend(rows(
        tx,
        "read the edits of a revoked message",
        "SELECT local FROM send_outcomes \
         WHERE conv = ?1 AND target = ?2 AND kind = 'edit' AND outcome = 'acked'",
        params![conv.encode(), target],
        |r| r.get::<_, String>(0),
    )?);
    let mut taken = take_copies(tx, conv, &ids)?;
    // The edits whose ack was never recorded are known by what their copy holds (fix F18).
    for form in started::forms(tx, conv)? {
        let Ok(chat) = jid_of(&form) else {
            continue;
        };
        let held = rows(
            tx,
            "read the sent copies of a chat",
            "SELECT message_id, payload FROM sent_messages WHERE chat_jid = ?1",
            params![chat.to_string()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?;
        for (id, payload) in held {
            if edits(&payload, target) {
                taken += exec(
                    tx,
                    "take the sent copy of an edit of a revoked message",
                    "DELETE FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2",
                    params![chat.to_string(), id],
                )?;
            }
        }
    }
    Ok(taken)
}

/// Takes the `sent_messages` rows of `ids` under every form of the chat.
fn take_copies(tx: &Transaction<'_>, conv: &ConvKey, ids: &[String]) -> Result<usize, StoreError> {
    let mut taken = 0;
    for form in started::forms(tx, conv)? {
        let Ok(chat) = jid_of(&form) else {
            continue;
        };
        let chat = chat.to_string();
        for id in ids {
            taken += exec(
                tx,
                "take the sent copy of a revoked message",
                "DELETE FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2",
                params![chat, id],
            )?;
        }
    }
    Ok(taken)
}

/// Whether the stored message `payload` is an EDIT of the message `target` (a protocol message of type
/// `MESSAGE_EDIT` keyed by it). A payload that does not decode is not.
fn edits(payload: &[u8], target: &str) -> bool {
    use waproto::buffa::Message as _;
    use waproto::whatsapp as wa;
    let Ok(message) = wa::Message::decode_from_slice(payload) else {
        return false;
    };
    message.protocol_message.as_option().is_some_and(|p| {
        p.r#type == Some(wa::message::protocol_message::Type::MessageEdit)
            && p.key
                .as_option()
                .is_some_and(|key| key.id.as_deref() == Some(target))
    })
}

/// Forgets the rows older than [`KEEP_MS`] before `now_ms`, in the caller's zone. Returns how many went.
pub(crate) fn sweep(tx: &Transaction<'_>, now_ms: i64) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget old send outcomes",
        "DELETE FROM send_outcomes WHERE at < ?1",
        [now_ms.saturating_sub(KEEP_MS)],
    )
}

#[cfg(test)]
mod tests;
