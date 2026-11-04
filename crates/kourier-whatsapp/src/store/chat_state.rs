//! The mirror of the phone's chat settings (W6a1, chunk A2; decisions D5, H5 and H6 of `target/wa/w6-decisions.md`):
//! what the phone's app state says about each chat, kept in `backend.db` (`chat_state`, migration 7) in the SAME zone
//! as the core events it causes.
//!
//! WHY A MIRROR. `PinnedConversations` is the full ordered list, so one pin event needs the others; a replay of a whole
//! collection (a snapshot, the link's full sync) says the same thing again and must reach `core_outbox` only where a
//! value really changed; the reconcile of a resync Kourier asked for needs a copy to find what the replay left out.
//!
//! ONE ROW PER (chat, field) ([`Field`]); the values are numbers (`archived`, `pinned`, `marked_unread`: 0 or 1;
//! `muted`: 0 for not muted, `i64::MAX` for ever, else the end in ms). The default of every field is [`DEFAULT`]: a chat
//! with no row is a chat with every field at its default, which is also what the core has for a chat it was told nothing
//! about. The latest ARRIVAL wins (upstream applies a patch only at the stored version + 1; times of different devices
//! are not compared), so [`put`] overwrites; `action_ms` is kept only for the pin order.
//!
//! WHAT THE CORE HAS (H5). A row says `stated` once the core was told its value. A chat the core does not know yet
//! (`stated: false` in the [`Put`]) keeps the value in the mirror and tells nobody: [`put`] answers `changed: false`, the
//! zone that first states the chat reads [`to_state`], writes the events and calls [`mark_stated`]. [`Written::changed`]
//! is therefore "the CORE must be told", compared with what the core has (the row's value when it was stated, else the
//! default), not with what the mirror had. A statement is never taken back by [`put`].
//!
//! GENERATIONS. `gen` is the generation of the row's collection when the row was last touched
//! ([`crate::store::resyncs`]): a resync Kourier asks for opens a new one, every row its replay touches takes it, and a
//! row left on an older one is what the replay did not mention. [`put`] stamps it every time, also when the value did
//! not change (a replay that says the same thing again is a touch). The reconcile (A5b, critic H1) is
//! [`unseen`] + [`reset`]: after a snapshot Kourier asked for came back synced and had mentioned at least one mutation
//! of the collection, the rows it left on an older generation are what the phone no longer has and go back to the default.
//!
//! The functions run inside the caller's zone (`&Transaction`) or read (`&Connection`); none logs.

use kourier_db::rusqlite::{Connection, Transaction, params};
use kourier_model::ConvKey;
use wacore::store::error::StoreError;

use super::{exec, first, rows};
use crate::error::unreadable;

/// The value every field has until the phone says otherwise.
pub(crate) const DEFAULT: i64 = 0;

/// The value of an archived chat (`Field::Archived`).
pub(crate) const ARCHIVED: i64 = 1;

/// `account_appstate` key: the phone brings an archived chat back on a new message (H6).
pub(crate) const UNARCHIVE_CHATS: &str = "unarchive_chats";

/// The settings of a chat the mirror keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    /// 1 = archived.
    Archived,
    /// 1 = pinned; `action_ms` is the order, newest first.
    Pinned,
    /// 0 = not muted, `i64::MAX` = for ever (the phone's -1), else the end in ms.
    Muted,
    /// 1 = marked unread on the phone.
    MarkedUnread,
}

impl Field {
    /// The column text.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Archived => "archived",
            Self::Pinned => "pinned",
            Self::Muted => "muted",
            Self::MarkedUnread => "marked_unread",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "archived" => Self::Archived,
            "pinned" => Self::Pinned,
            "muted" => Self::Muted,
            "marked_unread" => Self::MarkedUnread,
            _ => return None,
        })
    }
}

/// One row of the mirror, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) field: Field,
    pub(crate) value: i64,
    /// The mutation's own time (ms): the pin order.
    pub(crate) action_ms: i64,
    /// The generation of the collection when the row was last touched.
    pub(crate) generation: i64,
    /// The app-state collection the verb lives in.
    pub(crate) collection: String,
    /// The core was told the value.
    pub(crate) stated: bool,
}

/// One value the phone's app state gave for a chat.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Put<'a> {
    pub(crate) chat: &'a ConvKey,
    pub(crate) field: Field,
    pub(crate) value: i64,
    /// The mutation's own time (ms).
    pub(crate) action_ms: i64,
    /// The generation to stamp: the collection's current one (live) or the one a resync opened.
    pub(crate) generation: i64,
    pub(crate) collection: &'a str,
    /// The core knows the chat, so this value is stated now. `false`: the mirror keeps it and tells nobody (H5).
    pub(crate) stated: bool,
}

/// What a [`put`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Written {
    /// The core must be told: the put is stated and its value is not the one the core has (the stated row's value,
    /// else the default).
    pub(crate) changed: bool,
    /// The row existed and its `action_ms` moved (a pinned chat pinned again changes the pin order).
    pub(crate) retimed: bool,
}

/// Writes `put` (an insert, or an overwrite of the row's value, time, generation and collection) and says whether the
/// core must be told. A row that was stated stays stated.
pub(crate) fn put(tx: &Transaction<'_>, put: &Put<'_>) -> Result<Written, StoreError> {
    const OP: &str = "write a chat setting";
    let chat = put.chat.encode();
    let before = first(
        tx,
        OP,
        "SELECT value, action_ms, stated FROM chat_state WHERE chat = ?1 AND field = ?2",
        params![chat, put.field.as_str()],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        },
    )?;
    // What the core has: the value of a row it was told, else nothing, which is the default.
    let core = match before {
        Some((value, _, true)) => value,
        _ => DEFAULT,
    };
    exec(
        tx,
        OP,
        "INSERT INTO chat_state(chat, field, value, action_ms, gen, collection, stated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT (chat, field) DO UPDATE SET value = excluded.value, action_ms = excluded.action_ms, \
         gen = excluded.gen, collection = excluded.collection, stated = chat_state.stated OR excluded.stated",
        params![
            chat,
            put.field.as_str(),
            put.value,
            put.action_ms,
            put.generation,
            put.collection,
            i64::from(put.stated)
        ],
    )?;
    Ok(Written {
        changed: put.stated && put.value != core,
        retimed: before.is_some_and(|(_, action_ms, _)| action_ms != put.action_ms),
    })
}

const ENTRY: &str = "SELECT field, value, action_ms, gen, collection, stated FROM chat_state";

/// A row as the database gave it (the field is still text).
type Raw = (String, i64, i64, i64, String, bool);

fn raw(r: &kourier_db::rusqlite::Row<'_>) -> kourier_db::rusqlite::Result<Raw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get::<_, i64>(5)? != 0,
    ))
}

fn read_entry(
    (field, value, action_ms, generation, collection, stated): Raw,
) -> Result<Entry, StoreError> {
    Ok(Entry {
        field: Field::parse(&field).ok_or_else(|| unreadable("a chat setting row"))?,
        value,
        action_ms,
        generation,
        collection,
        stated,
    })
}

/// The row of `chat` and `field`, if the phone ever said anything about it.
pub(crate) fn get(
    conn: &Connection,
    chat: &ConvKey,
    field: Field,
) -> Result<Option<Entry>, StoreError> {
    first(
        conn,
        "look up a chat setting",
        &format!("{ENTRY} WHERE chat = ?1 AND field = ?2"),
        params![chat.encode(), field.as_str()],
        raw,
    )?
    .map(read_entry)
    .transpose()
}

/// The rows of `chat` the core still has to hear: not stated and not at the default. What the zone that first
/// states the chat writes as events (H5), in field order.
pub(crate) fn to_state(conn: &Connection, chat: &ConvKey) -> Result<Vec<Entry>, StoreError> {
    rows(
        conn,
        "look up the unstated chat settings",
        &format!("{ENTRY} WHERE chat = ?1 AND stated = 0 AND value != ?2 ORDER BY field"),
        params![chat.encode(), DEFAULT],
        raw,
    )?
    .into_iter()
    .map(read_entry)
    .collect()
}

/// A row a snapshot did not mention: still on an older generation, not at the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unseen {
    pub(crate) chat: ConvKey,
    pub(crate) field: Field,
    /// The core was told this value (so it must be told the default when the row is reset).
    pub(crate) stated: bool,
}

/// The rows of `collection` the snapshot of generation `generation` did not touch (an older generation) whose value is
/// not the default, by chat and field. A row already at the default needs nothing.
pub(crate) fn unseen(
    conn: &Connection,
    collection: &str,
    generation: i64,
) -> Result<Vec<Unseen>, StoreError> {
    rows(
        conn,
        "look up the chat settings a snapshot did not mention",
        "SELECT chat, field, stated FROM chat_state WHERE collection = ?1 AND gen < ?2 AND value != ?3 \
         ORDER BY chat, field",
        params![collection, generation, DEFAULT],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0)),
    )?
    .into_iter()
    .map(|(chat, field, stated)| {
        Ok(Unseen {
            chat: ConvKey::parse(&chat).map_err(|_| unreadable("a chat setting row"))?,
            field: Field::parse(&field).ok_or_else(|| unreadable("a chat setting row"))?,
            stated,
        })
    })
    .collect()
}

/// Puts one row back to the default and stamps it with `generation` (the reconcile's; the statement stays as it was).
pub(crate) fn reset(
    tx: &Transaction<'_>,
    chat: &ConvKey,
    field: Field,
    generation: i64,
) -> Result<(), StoreError> {
    exec(
        tx,
        "put a chat setting back to its default",
        "UPDATE chat_state SET value = ?3, gen = ?4 WHERE chat = ?1 AND field = ?2",
        params![chat.encode(), field.as_str(), DEFAULT, generation],
    )
    .map(drop)
}

/// Marks every row of `chat` stated (the zone that wrote the events calls it); the number of rows that changed, so a
/// second call says 0.
pub(crate) fn mark_stated(tx: &Transaction<'_>, chat: &ConvKey) -> Result<usize, StoreError> {
    exec(
        tx,
        "mark the chat settings stated",
        "UPDATE chat_state SET stated = 1 WHERE chat = ?1 AND stated = 0",
        params![chat.encode()],
    )
}

/// The chats the core knows that are pinned, newest pin first (the full list `PinnedConversations` carries). A pin of a
/// chat the core does not know stays out until that chat is stated (H5). Equal times order by the chat key, so the
/// list does not depend on the storage order.
pub(crate) fn pinned(conn: &Connection) -> Result<Vec<ConvKey>, StoreError> {
    rows(
        conn,
        "list the pinned chats",
        "SELECT chat FROM chat_state WHERE field = ?1 AND value != ?2 AND stated = 1 \
         ORDER BY action_ms DESC, chat",
        params![Field::Pinned.as_str(), DEFAULT],
        |r| r.get::<_, String>(0),
    )?
    .iter()
    .map(|chat| ConvKey::parse(chat).map_err(|_| unreadable("a chat setting row")))
    .collect()
}

/// Whether the core knows the chat: its features were stated at a version some build states (a `Rebuild` of the core
/// sets the stored version to 0, and the start sweep states the chats again, so for that time nothing is known). The
/// phone's value for a chat the core does not know waits in the mirror (H5). The EXACT key: a person the core holds
/// under the other form of his id (`pn:` and `lid:`) is another key here, so a setting named by the form the core has
/// not stated waits unstated rather than make a second, empty chat (R1's `settings` step shows which form the phone
/// writes).
pub(crate) fn core_knows(conn: &Connection, chat: &ConvKey) -> Result<bool, StoreError> {
    Ok(first(
        conn,
        "look whether the core knows a chat",
        "SELECT 1 FROM features_stated WHERE conv = ?1 AND version > 0",
        params![chat.encode()],
        |r| r.get::<_, i64>(0),
    )?
    .is_some())
}

/// A `Rebuild` of the core lost everything it was told: every row is unstated again (the start sweep states the chats
/// and, with each chat, its settings). The number of rows changed.
pub(crate) fn forget_statements(tx: &Transaction<'_>) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget the stated chat settings",
        "UPDATE chat_state SET stated = 0 WHERE stated = 1",
        [],
    )
}

/// Notes in the caller's zone that a message of `chat` that counts (the module doc of `appstate::zone`) has the sent time
/// `at_ms`: the newest one is kept (`chat_counted`, F1). One write per chat and zone.
pub(crate) fn note_counted(
    tx: &Transaction<'_>,
    chat: &ConvKey,
    at_ms: i64,
) -> Result<(), StoreError> {
    exec(
        tx,
        "note the newest counting message of a chat",
        "INSERT INTO chat_counted(chat, newest_ms) VALUES (?1, ?2) \
         ON CONFLICT (chat) DO UPDATE SET newest_ms = max(chat_counted.newest_ms, excluded.newest_ms)",
        params![chat.encode(), at_ms],
    )
    .map(drop)
}

/// The sent time (ms) of the newest message of `chat` that counted, `None` when none was delivered.
pub(crate) fn newest_counted(conn: &Connection, chat: &ConvKey) -> Result<Option<i64>, StoreError> {
    first(
        conn,
        "look up the newest counting message of a chat",
        "SELECT newest_ms FROM chat_counted WHERE chat = ?1",
        params![chat.encode()],
        |r| r.get::<_, i64>(0),
    )
}

/// Notes in the caller's zone that the history of `chat` was cleared or deleted up to `cut_ms` (`chat_cut`, F3): only ever
/// raised, so an older clear that is replayed lowers nothing.
pub(crate) fn raise_cut(
    tx: &Transaction<'_>,
    chat: &ConvKey,
    cut_ms: i64,
) -> Result<(), StoreError> {
    exec(
        tx,
        "note the cut of a chat",
        "INSERT INTO chat_cut(chat, cut_ms) VALUES (?1, ?2) \
         ON CONFLICT (chat) DO UPDATE SET cut_ms = max(chat_cut.cut_ms, excluded.cut_ms)",
        params![chat.encode(), cut_ms],
    )
    .map(drop)
}

/// The time (ms) up to which `chat`'s history was cleared or deleted, as far as the backend knows; `None` when it knows
/// of no clear.
pub(crate) fn cut_of(conn: &Connection, chat: &ConvKey) -> Result<Option<i64>, StoreError> {
    first(
        conn,
        "look up the cut of a chat",
        "SELECT cut_ms FROM chat_cut WHERE chat = ?1",
        params![chat.encode()],
        |r| r.get::<_, i64>(0),
    )
}

/// An archive of the mirror that a message which counts is newer than.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaleArchive {
    pub(crate) chat: ConvKey,
    /// The core was told the chat is archived.
    pub(crate) stated: bool,
}

/// The chats the mirror holds archived although a message that counts is newer than the archive's time (`action_ms`),
/// by chat. What a phone with "Keep chats archived" OFF has moved out of the archive without a word (F1).
pub(crate) fn stale_archives(conn: &Connection) -> Result<Vec<StaleArchive>, StoreError> {
    rows(
        conn,
        "look up the archives a newer message made stale",
        "SELECT s.chat, s.stated FROM chat_state s JOIN chat_counted c ON c.chat = s.chat \
         WHERE s.field = ?1 AND s.value != ?2 AND c.newest_ms > s.action_ms ORDER BY s.chat",
        params![Field::Archived.as_str(), DEFAULT],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)),
    )?
    .into_iter()
    .map(|(chat, stated)| {
        Ok(StaleArchive {
            chat: ConvKey::parse(&chat).map_err(|_| unreadable("a chat setting row"))?,
            stated,
        })
    })
    .collect()
}

/// Puts the `archived` row of `chat` back to the default (a stale archive the zone has just refused; the statement and
/// the time stay as they were).
pub(crate) fn clear_archive(tx: &Transaction<'_>, chat: &ConvKey) -> Result<(), StoreError> {
    exec(
        tx,
        "put a stale archive back to the default",
        "UPDATE chat_state SET value = ?3 WHERE chat = ?1 AND field = ?2",
        params![chat.encode(), Field::Archived.as_str(), DEFAULT],
    )
    .map(drop)
}

/// Puts the `archived` row of `chat` back to archived: the undo of [`clear_archive`] for a history row the core refused
/// (G2, NEW-2). The zone had cleared the row because it told the core `archived: false`; a refused row told it nothing,
/// so the core still holds the chat archived and the mirror, which records what the core has, must say so too (the time
/// and the statement stay as they were). A row that is not at the default (the phone archived the chat again meanwhile)
/// is left alone.
pub(crate) fn restore_archive(tx: &Transaction<'_>, chat: &ConvKey) -> Result<(), StoreError> {
    exec(
        tx,
        "put the archive of a refused unarchive back",
        "UPDATE chat_state SET value = ?3 WHERE chat = ?1 AND field = ?2 AND value = ?4",
        params![chat.encode(), Field::Archived.as_str(), ARCHIVED, DEFAULT],
    )
    .map(drop)
}

/// An account-level setting of the phone (`UNARCHIVE_CHATS`), `None` until the phone said one.
pub(crate) fn account_flag(conn: &Connection, key: &str) -> Result<Option<bool>, StoreError> {
    Ok(first(
        conn,
        "look up an account setting",
        "SELECT value FROM account_appstate WHERE key = ?1",
        params![key],
        |r| r.get::<_, i64>(0),
    )?
    .map(|value| value != 0))
}

/// Sets an account-level setting; `true` when it is new or differs from the one stored.
pub(crate) fn set_account_flag(
    tx: &Transaction<'_>,
    key: &str,
    value: bool,
) -> Result<bool, StoreError> {
    const OP: &str = "write an account setting";
    let before = account_flag(tx, key)?;
    exec(
        tx,
        OP,
        "INSERT INTO account_appstate(key, value) VALUES (?1, ?2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, i64::from(value)],
    )?;
    Ok(before != Some(value))
}

#[cfg(test)]
mod tests;
