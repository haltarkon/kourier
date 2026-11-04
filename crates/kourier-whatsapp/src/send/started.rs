//! D6 in the backend (W5a, decision D7, critic B1): which DIRECT chats were started on the phone, and the
//! features each chat was last stated with.
//!
//! THE FACT. A companion's first message into a chat nobody started is refused by the server (463) AND counts
//! against the account, and his rule is "never; start this chat on your phone". So the backend keeps, in
//! `backend.db` (`chat_started`, migration 5), the fact that a 1:1 chat holds something that proves the chat
//! exists on the phone, written in the zone that sees it ([`How`]): a message from the peer and an own message
//! from another of our devices (stage 2's zone, [`note_messages`]), a history chat of which the phone pushed at
//! least one REAL message of either side (W4's delivery zone, fix F2: a chats row alone is no start; the same
//! [`proves_start`] rule as the live path), an own message the server acked (the ledger's zone, `send::ledger`).
//! NOT a call line or a call record, a typing signal, a receipt, a presence or an `IdentityMerged`: a stranger's
//! call makes a 1:1 conversation in the core and is no start. A group is always started (its membership is the server's), so groups have no
//! row and [`holds`] says yes for them. Until the fact holds the chat states `cannot_send: NotAccepted` and the
//! backend refuses every Transmit, typing and receipt for it with no frame (the actor's job, chunk 9c).
//!
//! ONE PERSON, TWO IDS (fix F3). The core may hold a 1:1 chat under the number while the fact was noted under the
//! LID (or the other way). A number can have several LIDs over time (it is re-registered: the old LID still
//! answers), and the frame goes to the number's CURRENT LID (`known::load`: the newest mapping), so [`holds`] asks
//! the destination's own resolution ([`start_forms`]): for a `pn:` chat the number's row and its current LID's
//! row; for a `lid:` chat its own row, and its number's row only while this LID is that number's current LID. A
//! fact lent to every LID a number ever had would open the chat to a stranger who got the number later. New
//! facts are noted under the LID when one is known ([`note`]). [`forms`] (every form the mapping knows) is for
//! what is DELETED or merely looked for (the sent copies of a revoked message, a token), never for a send. A form
//! the mapping does not know yet errs toward "not started" (a refusal the person can lift on the phone), never
//! toward a send.
//!
//! THE STATED FEATURES (`features_stated`): a chat is stated once per (features version, started fact);
//! [`needs_statement`] / [`mark_stated`] in the zone that states it, [`forget_statements`] at a `Rebuild`
//! start (critic M4). The statement itself is `features`'.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::rusqlite::{Connection, Transaction, params};
use kourier_model::{Body, ConvKey, Origin, UserId};
use wacore::store::error::StoreError;

use super::known;
use crate::store::{exec, first, rows};

/// Why a direct chat counts as started (the `how` column; the first reason is kept).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum How {
    /// A message from the peer reached us.
    Peer = 0,
    /// An own message from another of our devices (the phone's echo).
    OwnOtherDevice = 1,
    /// The phone's history push named the chat.
    History = 2,
    /// The server acked a message Kourier sent into it.
    Acked = 3,
}

/// Records that `conv` was started, for a direct chat (anything else is always started and gets no row). A
/// number's fact is noted under its current LID when the store knows one (fix F3: the LID is what the frame goes
/// to). `true` when the row is new.
pub(crate) fn note(tx: &Transaction<'_>, conv: &ConvKey, how: How) -> Result<bool, StoreError> {
    let ConvKey::Direct(user) = conv else {
        return Ok(false);
    };
    let mut key = conv.encode();
    if user.scheme() == "pn"
        && let Some(lid) = known::current_lid(tx, user.value())?
        && let Ok(lid) = UserId::from_parts("lid", &lid)
    {
        key = ConvKey::Direct(lid).encode();
    }
    let changed = exec(
        tx,
        "note a chat started on the phone",
        "INSERT INTO chat_started(conv, how) VALUES (?1, ?2) ON CONFLICT (conv) DO NOTHING",
        params![key, how as i64],
    )?;
    Ok(changed > 0)
}

/// The kinds of message that are RECORDS OF A CALL, not something a person wrote (the class table maps them to an
/// unsupported body, `map/class.rs`): they prove no start.
const CALL_RECORDS: [&str; 3] = [
    "call_log_messsage",
    "bcall_message",
    "scheduled_call_creation_message",
];

/// Whether `message` proves that its direct chat was started on the phone: something a person wrote, from the peer
/// or from another device of ours. A system line (a call, a notice) and a record of a call prove nothing, and a
/// message of this device comes only from the ledger's zone, after the server's ack. THE ONE RULE of the live path
/// ([`note_messages`]) and the history's ([`crate::history::chat`], fix F2).
pub(crate) fn proves_start(message: &IncomingMessage) -> bool {
    match &message.body {
        Body::System(_) => return false,
        Body::Unsupported { kind_hint, .. } if CALL_RECORDS.contains(&kind_hint.as_str()) => {
            return false;
        }
        _ => {}
    }
    matches!(
        message.origin,
        Origin::Remote { .. } | Origin::OwnOtherDevice { .. }
    )
}

/// Notes the chats that `events` (the events of one finished inbox row) prove started: a message of the peer
/// or one of ours from another device, in a direct chat ([`proves_start`]). Returns how many rows were new.
pub(crate) fn note_messages(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
) -> Result<usize, StoreError> {
    let mut noted = 0;
    for event in events {
        let BackendEvent::Message(message) = event else {
            continue;
        };
        if !proves_start(message) {
            continue;
        }
        let how = match message.origin {
            Origin::Remote { .. } => How::Peer,
            _ => How::OwnOtherDevice,
        };
        noted += usize::from(note(tx, &message.conv, how)?);
    }
    Ok(noted)
}

/// `conv` and the other forms of the same person the store's mapping knows (a direct chat under `pn:` and its
/// `lid:`s, or under `lid:` and its `pn:`); a group, a list or a channel is only itself.
pub(crate) fn forms(conn: &Connection, conv: &ConvKey) -> Result<Vec<ConvKey>, StoreError> {
    let mut out = vec![conv.clone()];
    let ConvKey::Direct(user) = conv else {
        return Ok(out);
    };
    let (statement, other) = match user.scheme() {
        "pn" => ("SELECT lid FROM lid_pn_mapping WHERE phone = ?1", "lid"),
        "lid" => ("SELECT phone FROM lid_pn_mapping WHERE lid = ?1", "pn"),
        _ => return Ok(out),
    };
    let found = rows(
        conn,
        "read the other forms of a chat",
        statement,
        [user.value()],
        |r| r.get::<_, String>(0),
    )?;
    for value in found {
        if let Ok(user) = UserId::from_parts(other, &value) {
            let key = ConvKey::Direct(user);
            if !out.contains(&key) {
                out.push(key);
            }
        }
    }
    Ok(out)
}

/// The ids under which the fact of the direct chat `conv` is looked for: the DESTINATION's own resolution (fix F3).
/// A `pn:` chat is written to the number's current LID (`known::load`), so its facts are its own row and that LID's;
/// a `lid:` chat has its own row, and its number's row only while this LID is that number's current one. Another
/// LID of the same number (an older one of a recycled number) is another person's.
pub(crate) fn start_forms(conn: &Connection, conv: &ConvKey) -> Result<Vec<ConvKey>, StoreError> {
    let mut out = vec![conv.clone()];
    let ConvKey::Direct(user) = conv else {
        return Ok(out);
    };
    let other = match user.scheme() {
        "pn" => known::current_lid(conn, user.value())?
            .and_then(|lid| UserId::from_parts("lid", &lid).ok()),
        "lid" => match known::number_of(conn, user.value())? {
            Some(pn) if known::current_lid(conn, &pn)?.as_deref() == Some(user.value()) => {
                UserId::from_parts("pn", &pn).ok()
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(other) = other {
        out.push(ConvKey::Direct(other));
    }
    Ok(out)
}

/// Whether the chat was started on the phone: always for a group; for a direct chat when any of its
/// [`start_forms`] has a row; never for a list, a channel or a key of another kind.
pub(crate) fn holds(conn: &Connection, conv: &ConvKey) -> Result<bool, StoreError> {
    match conv {
        ConvKey::Group(_) => Ok(true),
        ConvKey::Broadcast(_) | ConvKey::Channel(_) => Ok(false),
        ConvKey::Direct(_) => {
            for form in start_forms(conn, conv)? {
                let found = first(
                    conn,
                    "look whether a chat was started on the phone",
                    "SELECT 1 FROM chat_started WHERE conv = ?1",
                    [form.encode()],
                    |r| r.get::<_, i64>(0),
                )?;
                if found.is_some() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Whether `conv` must be stated (again): it never was, or with another features version or started fact.
pub(crate) fn needs_statement(
    conn: &Connection,
    conv: &ConvKey,
    version: u32,
    started: bool,
) -> Result<bool, StoreError> {
    let stated = first(
        conn,
        "look how a chat was stated",
        "SELECT version, started FROM features_stated WHERE conv = ?1",
        [conv.encode()],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)),
    )?;
    Ok(stated != Some((i64::from(version), started)))
}

/// Whether `conv` was ever stated (a `features_stated` row of any version): the backend holds it as a chat of the account.
/// A row at version 0 is a chat whose statement a `Rebuild` of the core or a taken-back history row forgot; the start
/// sweep or the chat's next event states it again.
pub(crate) fn was_stated(conn: &Connection, conv: &ConvKey) -> Result<bool, StoreError> {
    Ok(first(
        conn,
        "look whether a chat was ever stated",
        "SELECT 1 FROM features_stated WHERE conv = ?1",
        [conv.encode()],
        |r| r.get::<_, i64>(0),
    )?
    .is_some())
}

/// Records how `conv` was stated, in the zone that states it.
pub(crate) fn mark_stated(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    version: u32,
    started: bool,
) -> Result<(), StoreError> {
    exec(
        tx,
        "record how a chat was stated",
        "INSERT INTO features_stated(conv, version, started) VALUES (?1, ?2, ?3) \
         ON CONFLICT (conv) DO UPDATE SET version = excluded.version, started = excluded.started",
        params![conv.encode(), i64::from(version), i64::from(started)],
    )
    .map(drop)
}

/// Forgets every statement: a `Rebuild` of the core lost them (critic M4). The rows stay with version 0, which no
/// build states, so the start sweep (`features::sweep`) states every chat stated before again, not only the
/// chats of the history.
pub(crate) fn forget_statements(tx: &Transaction<'_>) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget the stated features",
        "UPDATE features_stated SET version = 0",
        [],
    )
}

#[cfg(test)]
mod tests;
