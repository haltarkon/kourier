//! The phone's chat settings in the contract's words (W6a1, chunk A3; decisions D5, H5 and H6 of
//! `target/wa/w6-decisions.md`). PURE: one typed app-state event of upstream in, what it says out; no store, no clock,
//! no log. The handler's zone (A4) writes the mirror ([`crate::store::chat_state`]) and the core events in ONE
//! transaction and decides, with the mirror, what changed.
//!
//! WHAT IS MAPPED (the table of D5): `archive` -> the mirror's `archived` (an archive is dated by the last message of its
//! range, F1: [`archive_range_ms`]); `pin_v1` -> `pinned`, ordered by the
//! mutation's own time, newest first (the full list is built from the mirror, known chats only: [`core_event`] says
//! nothing about a pin); `mute` -> `muted` (the phone's `-1` is for ever = `i64::MAX`; a time is milliseconds, as
//! written; not muted, or an end of 0 or none, is 0: whatsmeow's reading); `markChatAsRead` `read: false` ->
//! `marked_unread`, `read: true` -> unmarked AND a [`BackendEvent::ReadMarker`] up to the newest message of its range
//! (none when the range names none: read-by-time is the known limit of D5); `clearChat` and `deleteChat` ->
//! [`BackendEvent::ConversationCleared`] with the range's keys and `before_ms` = the newer of the range's two times
//! (seconds) times 1000, `remove` only for the delete (the core keeps the largest claim per chat and never lowers it, so
//! a replay changes nothing; a starred message the phone kept goes too, Kourier has no stars); `deleteMessageForMe` ->
//! [`BackendEvent::Deleted`] for me; `setting_unarchiveChats` -> the account's flag (H6). Everything else in the
//! collections (star, lock, labels, the status mute, other settings) maps to nothing.
//!
//! The chat of an event goes through `map::conv_of`: a person (`pn:`, `lid:`) or a group; a status, a list or a
//! channel is no conversation of the contract's and the event maps to nothing. Which form of a person's id the phone
//! writes is only shown by a real run (R1).
//!
//! `collection` is the collection the verb lives in (from upstream's own schema table), returned for EVERY mapped
//! event, also one that only has core events: the reconcile (A5b) counts what a replay mentioned per collection.
//!
//! Nothing here formats an upstream value: no JID, number or id reaches a log or an error.

use kourier_backend_api::BackendEvent;
use kourier_model::ids::validate_local;
use kourier_model::{ConvKey, ConversationPatch, MsgTarget, Patch, TargetAuthor};
use wacore::appstate::schemas;
use wacore::types::events::Event;
use wacore_binary::Jid;
use waproto::whatsapp as wa;

use crate::map;
use crate::store::chat_state::Field;

/// The collections of the verbs this map reads, from upstream's schema table.
pub(crate) const ARCHIVE: &str = schemas::ARCHIVE.collection.as_str();
const PIN: &str = schemas::PIN.collection.as_str();
const MUTE: &str = schemas::MUTE.collection.as_str();
const MARK_AS_READ: &str = schemas::MARK_CHAT_AS_READ.collection.as_str();
const CLEAR_CHAT: &str = schemas::CLEAR_CHAT.collection.as_str();
const DELETE_CHAT: &str = schemas::DELETE_CHAT.collection.as_str();
const DELETE_FOR_ME: &str = schemas::DELETE_MESSAGE_FOR_ME.collection.as_str();
const UNARCHIVE_SETTING: &str = schemas::UNARCHIVE_CHATS_SETTING.collection.as_str();
/// The collection of the address book's `contact` verb (W6a2, B8b).
pub(crate) const CONTACT: &str = schemas::CONTACT.collection.as_str();
/// The collection of the call log (A6: written by `calls::log`, not mapped here).
pub(crate) const CALL_LOG: &str = schemas::CALL_LOG.collection.as_str();

/// A value of one chat's setting, for the mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Setting {
    pub(crate) chat: ConvKey,
    pub(crate) field: Field,
    /// The mirror's number (`store::chat_state`): 0 or 1, or the mute's end.
    pub(crate) value: i64,
    /// The mutation's own time (ms): the pin order.
    pub(crate) action_ms: i64,
}

/// What an event says for the mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mirror {
    /// One chat's setting.
    Chat(Setting),
    /// The account's "a new message unarchives its chat" (H6).
    Unarchive(bool),
    /// One contact of the phone's address book (W6a2, B8b): the mirror and the names of the people the core knows.
    Book(crate::book::Change),
}

/// What one app-state event says.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mapped {
    /// The collection the verb lives in.
    pub(crate) collection: &'static str,
    /// The mirror's side, if the event has one.
    pub(crate) mirror: Option<Mirror>,
    /// Core events that do not depend on the mirror, in order (a read marker, a clear, a delete for me).
    pub(crate) events: Vec<BackendEvent>,
    /// Upstream's provenance of the event: `true` for a record of a SNAPSHOT replay (the link's full sync, a recovery,
    /// our own start-up resync), `false` for a patch (an incremental sync, a live change). The reconcile (H1) counts
    /// only the first kind as "the replay touched this collection": a patch applied while the snapshot waits for its
    /// turn is not the snapshot (DUR-1).
    pub(crate) from_full_sync: bool,
}

impl Mapped {
    /// Whether the event is a change of the phone's address book (W6a2, B9: it can make a merge due).
    pub(crate) fn is_address_book(&self) -> bool {
        matches!(self.mirror, Some(Mirror::Book(_)))
    }

    fn chat(
        collection: &'static str,
        chat: ConvKey,
        field: Field,
        value: i64,
        action_ms: i64,
    ) -> Self {
        Self {
            collection,
            mirror: Some(Mirror::Chat(Setting {
                chat,
                field,
                value,
                action_ms,
            })),
            events: Vec::new(),
            from_full_sync: false,
        }
    }

    fn book(change: crate::book::Change) -> Self {
        Self {
            collection: CONTACT,
            mirror: Some(Mirror::Book(change)),
            events: Vec::new(),
            from_full_sync: false,
        }
    }

    fn events(collection: &'static str, events: Vec<BackendEvent>) -> Self {
        Self {
            collection,
            mirror: None,
            events: checked(events),
            from_full_sync: false,
        }
    }
}

/// What the phone's app-state event `event` says; `None` for every event this map does not read (and for a chat that
/// is no conversation of the contract's).
pub(crate) fn map(event: &Event) -> Option<Mapped> {
    let mut mapped = read(event)?;
    mapped.from_full_sync = from_full_sync(event);
    Some(mapped)
}

/// Upstream's own word on where the event came from, for the events [`read`] reads.
fn from_full_sync(event: &Event) -> bool {
    match event {
        Event::ArchiveUpdate(u) => u.from_full_sync,
        Event::PinUpdate(u) => u.from_full_sync,
        Event::MuteUpdate(u) => u.from_full_sync,
        Event::MarkChatAsReadUpdate(u) => u.from_full_sync,
        Event::ClearChatUpdate(u) => u.from_full_sync,
        Event::DeleteChatUpdate(u) => u.from_full_sync,
        Event::DeleteMessageForMeUpdate(u) => u.from_full_sync,
        Event::UnarchiveChatsSettingUpdate(u) => u.from_full_sync,
        Event::ContactUpdate(u) => u.from_full_sync,
        Event::ContactRemoved(u) => u.from_full_sync,
        _ => false,
    }
}

fn read(event: &Event) -> Option<Mapped> {
    match event {
        Event::ArchiveUpdate(u) => {
            let chat = map::conv_of(&u.jid)?;
            let archived = u.action.archived.unwrap_or(false);
            let at = u.action_timestamp.unwrap_or(u.timestamp).timestamp_millis();
            // An archive is as new as the last message of its range (F1, INTEROP-1): the phone writes nothing when its
            // own setting moves a chat out of the archive, so the record keeps naming the old range. Seconds, as the
            // clear's range; none (or none usable): the action's own time.
            let at = if archived {
                archive_range_ms(u.action.message_range.as_option()).unwrap_or(at)
            } else {
                at
            };
            Some(Mapped::chat(
                ARCHIVE,
                chat,
                Field::Archived,
                i64::from(archived),
                at,
            ))
        }
        Event::PinUpdate(u) => {
            let chat = map::conv_of(&u.jid)?;
            let pinned = u.action.pinned.unwrap_or(false);
            let at = u.action_timestamp.unwrap_or(u.timestamp).timestamp_millis();
            Some(Mapped::chat(
                PIN,
                chat,
                Field::Pinned,
                i64::from(pinned),
                at,
            ))
        }
        Event::MuteUpdate(u) => {
            let chat = map::conv_of(&u.jid)?;
            let muted = muted_until(
                u.action.muted.unwrap_or(false),
                u.action.mute_end_timestamp.unwrap_or(0),
            );
            let at = u.action_timestamp.unwrap_or(u.timestamp).timestamp_millis();
            Some(Mapped::chat(MUTE, chat, Field::Muted, muted, at))
        }
        Event::MarkChatAsReadUpdate(u) => {
            let chat = map::conv_of(&u.jid)?;
            // Only an explicit answer counts: a record that says neither is read as nothing.
            let read = u.action.read?;
            let at = u.action_timestamp.unwrap_or(u.timestamp).timestamp_millis();
            let mut mapped = Mapped::chat(
                MARK_AS_READ,
                chat.clone(),
                Field::MarkedUnread,
                i64::from(!read),
                at,
            );
            if read
                && let Some(range) = u.action.message_range.as_option()
                && let Some(up_to) = newest(&chat, range)
            {
                mapped.events = checked(vec![BackendEvent::ReadMarker {
                    conv: Some(chat),
                    up_to,
                }]);
            }
            Some(mapped)
        }
        Event::ClearChatUpdate(u) => {
            let chat = map::conv_of(&u.jid)?;
            let at = u.action_timestamp.unwrap_or(u.timestamp).timestamp_millis();
            Some(Mapped::events(
                CLEAR_CHAT,
                cleared(&chat, u.action.message_range.as_option(), false, at),
            ))
        }
        Event::DeleteChatUpdate(u) => {
            let chat = map::conv_of(&u.jid)?;
            let at = u.action_timestamp.unwrap_or(u.timestamp).timestamp_millis();
            Some(Mapped::events(
                DELETE_CHAT,
                cleared(&chat, u.action.message_range.as_option(), true, at),
            ))
        }
        Event::DeleteMessageForMeUpdate(u) => {
            let chat = map::conv_of(&u.chat_jid)?;
            let target = own_view_target(
                &chat,
                u.from_me,
                u.participant_jid.as_ref(),
                Some(u.message_id.as_str()),
            )?;
            let at = u.action_timestamp.unwrap_or(u.timestamp).timestamp_millis();
            Some(Mapped::events(
                DELETE_FOR_ME,
                vec![BackendEvent::Deleted {
                    target,
                    by: None,
                    for_everyone: false,
                    at,
                }],
            ))
        }
        Event::UnarchiveChatsSettingUpdate(u) => Some(Mapped {
            collection: UNARCHIVE_SETTING,
            mirror: Some(Mirror::Unarchive(u.unarchive_chats)),
            events: Vec::new(),
            from_full_sync: false,
        }),
        // The address book (B8b): the mirror holds what the phone said; the zone states a name for a person the core knows.
        Event::ContactUpdate(u) => {
            let change = crate::book::change_of_action(&u.jid, &u.action)?;
            Some(Mapped::book(change))
        }
        Event::ContactRemoved(u) => {
            let change = crate::book::change_of_removal(&u.jid)?;
            Some(Mapped::book(change))
        }
        _ => None,
    }
}

/// The mirror's number for a mute: 0 = not muted; a negative end = for ever (`i64::MAX`, the phone's `-1`); a time
/// stays as written (milliseconds); a mute with no end is none (whatsmeow reads an end of 0 as already over).
fn muted_until(muted: bool, end: i64) -> i64 {
    if !muted {
        0
    } else if end < 0 {
        i64::MAX
    } else {
        end
    }
}

/// The core's words for a changed value of `field` in `chat`. A pin has none here: the core hears the FULL list
/// (`PinnedConversations`, built from the mirror), so one pin event needs the others.
pub(crate) fn core_event(chat: &ConvKey, field: Field, value: i64) -> Option<BackendEvent> {
    let mut patch = ConversationPatch::new(chat.clone());
    match field {
        Field::Archived => patch.archived = Some(value != 0),
        Field::MarkedUnread => patch.marked_unread = Some(value != 0),
        Field::Muted => {
            patch.muted_until = if value == 0 {
                Patch::Clear
            } else {
                Patch::Set(value)
            }
        }
        Field::Pinned => return None,
    }
    Some(BackendEvent::ConversationUpsert(patch))
}

/// A clear (`remove: false`) or a delete (`remove: true`) of a chat by a range. No bound at all (no key and no time)
/// says nothing the core could use, so it gives no event.
fn cleared(
    chat: &ConvKey,
    range: Option<&wa::sync_action_value::SyncActionMessageRange>,
    remove: bool,
    at: i64,
) -> Vec<BackendEvent> {
    let Some(range) = range else {
        return Vec::new();
    };
    let up_to: Vec<MsgTarget> = range
        .messages
        .iter()
        .filter_map(|m| m.key.as_option())
        .filter_map(|key| target(chat, key))
        .collect();
    let before_ms = before_ms(range);
    if up_to.is_empty() && before_ms.is_none() {
        return Vec::new();
    }
    vec![BackendEvent::ConversationCleared {
        conv: chat.clone(),
        up_to,
        remove,
        before_ms,
        at,
    }]
}

/// The time of the last message of an archive's range, in milliseconds (WhatsApp writes seconds); `None` when the range
/// is missing or names none.
fn archive_range_ms(range: Option<&wa::sync_action_value::SyncActionMessageRange>) -> Option<i64> {
    range?
        .last_message_timestamp
        .filter(|seconds| *seconds > 0)?
        .checked_mul(1000)
}

/// The clear-by-time claim: the newer of the range's two times, which WhatsApp writes in SECONDS, in milliseconds.
fn before_ms(range: &wa::sync_action_value::SyncActionMessageRange) -> Option<i64> {
    let newest = range
        .last_message_timestamp
        .unwrap_or(0)
        .max(range.last_system_message_timestamp.unwrap_or(0));
    if newest > 0 {
        newest.checked_mul(1000)
    } else {
        None
    }
}

/// The newest message of a range that names a usable key: the one with the latest time, the later of two equal.
fn newest(
    chat: &ConvKey,
    range: &wa::sync_action_value::SyncActionMessageRange,
) -> Option<MsgTarget> {
    range
        .messages
        .iter()
        .filter_map(|m| {
            let target = target(chat, m.key.as_option()?)?;
            Some((m.timestamp.unwrap_or(0), target))
        })
        .max_by_key(|(time, _)| *time)
        .map(|(_, target)| target)
}

/// The message a key of an own-phone record names. `None` when the key states no valid id.
fn target(chat: &ConvKey, key: &wa::MessageKey) -> Option<MsgTarget> {
    let participant = key
        .participant
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<Jid>().ok());
    own_view_target(
        chat,
        key.from_me == Some(true),
        participant.as_ref(),
        key.id.as_deref(),
    )
}

/// A message of `chat` as OUR OWN phone's record names it: ours (`from_me`), the peer's in a 1:1 chat, or a group
/// member's by the key's `participant`.
fn own_view_target(
    chat: &ConvKey,
    from_me: bool,
    participant: Option<&Jid>,
    id: Option<&str>,
) -> Option<MsgTarget> {
    let local = id.filter(|id| validate_local(id).is_ok())?;
    let author = if from_me {
        TargetAuthor::Own
    } else {
        match chat {
            ConvKey::Direct(peer) => TargetAuthor::User(peer.clone()),
            _ => participant
                .and_then(map::person_of)
                .map_or(TargetAuthor::Unknown, TargetAuthor::User),
        }
    };
    Some(MsgTarget {
        conv: Some(chat.clone()),
        author,
        local: local.into(),
        alternates: Vec::new(),
    })
}

/// The events as they are when the contract takes every id in them, else none (the mirror side of the event stays).
fn checked(events: Vec<BackendEvent>) -> Vec<BackendEvent> {
    if events.iter().all(|event| event.validate_ids().is_ok()) {
        events
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests;
