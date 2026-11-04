//! The zones that act on the phone's chat settings (W6a1, chunk A4; decisions D4-D6, H5, H6 and M6 of
//! `target/wa/w6-decisions.md`, critic B1 point 3). Everything here runs INSIDE the caller's `backend.db` transaction
//! and logs nothing; the caller decides what a failure means (the handler's: it ends the connection).
//!
//! FOUR ZONES:
//!
//! - [`apply`]: one mapped app-state event ([`map::Mapped`]) of the handler. The mirror write ([`chat_state::put`]) and
//!   the core events it causes (and, for the phone's clear or delete of a chat, the chat's upload records going and its cut noted, F3) are one transaction (the group commit of [`crate::events::EventWriter`], held by the
//!   link hold like every handler write: M6). A value the core already has gives NO event, so a replay of a whole
//!   collection (a snapshot, the link's full sync) reaches `core_outbox` only where something changed. The full pin list
//!   is built from the mirror, known chats only.
//! - [`settings_of`]: the zone that FIRST states a chat (`features::state`) also states what the phone said about it
//!   before (H5): the mirror's unstated rows as events, then the pin list when a pin was among them, right after the
//!   chat's statement and before its first event. A chat the core does not know gets no event of its own: a value for it
//!   waits in the mirror, so the phone's old settings never make an empty chat in the list.
//! - [`with_unarchive`] (H6): "Keep chats archived" is the PHONE's setting, applied by each device itself, so when it
//!   says a new message brings a chat back, the zone of a counting message puts `archived: Some(false)` in front of it
//!   (the same counting rule as Signal's A1, `receive/unarchive.rs`) and the mirror follows. Never written to the phone.
//!   THE TIMES (F1, INTEROP-1, DUR-3): the phone writes NOTHING when its own setting moves a chat out of the archive, so
//!   its `archive` record keeps naming the old range. An archive is dated by the last message of its range (the map), and
//!   Kourier notes per chat the sent time of the newest message that counted it delivered (`chat_counted`: [`prepare`]
//!   for the live zones, [`after_history`] for the history's, one write per chat and zone). While the setting says
//!   unarchive, an archive that a counting message is STRICTLY newer than is stale ([`refuses_archive`]): [`apply`] and
//!   [`settings_of`] keep the default, and the setting arriving after the records re-checks them. A message brings a
//!   chat back only when it is strictly newer than the archive (a message that was last when the user archived has the
//!   archive's own time and brings nothing back, in either order of arrival) and the row takes the message's time. A CALL
//!   line is never the noted time (G1): the archive's range is dated by `lastMessageTimestamp`, and a call is on the
//!   range's system side; a live call into an archived chat still brings it back here (A1's rule).
//! - [`reconcile`] (A5b, critic H1 and M8): after a snapshot Kourier asked for came back synced AND mentioned at least one
//!   mutation of its collection, the rows of that collection the replay left on an older generation are what the phone no
//!   longer has: each goes back to the default and the core is told (the full pin list when a pin went). A row with an
//!   open intent of W6b is never reset ([`has_open_intent`]). The address book is reconciled the same way (F19, DUR-6): the
//!   contacts of `critical_unblock_low` the replay left on an older generation are taken back as a REMOVE takes them
//!   ([`crate::book::reconcile`]). The caller decides WHETHER to reconcile; an empty replay,
//!   a snapshot upstream started itself and a collection that lost an event never reach this function.
//!
//! WHAT COUNTS as "a counting message" ([`message_counts`]): a person's message unless it arrives already read, a message
//! of ours from the phone (the transcript) and a call line; not a notice, a placeholder, a reaction, an edit, a delete, a
//! read marker or a receipt (those are no `Message`), and what the history push delivers never unarchives (its zone does
//! not call [`with_unarchive`]). A call line that the call log wrote is NOT one of those: the log's lines are quiet
//! history (W6a1 A6 keeps them out of this path). The history push's rows are all `read: true` (the chat carries the
//! unread count), so for the NOTED time ([`after_history`]) a message of somebody else counts whatever `read` says.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::rusqlite::Transaction;
use kourier_model::{Body, ConvKey, Origin, SystemEvent};
use wacore::store::error::StoreError;

use super::map::{self, Mapped, Mirror};
use crate::store::chat_state::{self, DEFAULT, Field, Put};
use crate::store::resyncs;

/// The generation the live puts of `collection` are stamped with: the one the last snapshot Kourier asked for opened (0
/// before any).
fn generation(tx: &Transaction<'_>, collection: &str) -> Result<i64, StoreError> {
    Ok(resyncs::book(tx, collection)?.map_or(0, |book| book.generation))
}

/// The handler's zone for one mapped app-state event: writes the mirror and returns the events for the core, in order
/// (empty when nothing changed). The events of the map itself (a read marker, a clear, a delete for me) follow the
/// ones the mirror causes.
pub(crate) fn apply(
    tx: &Transaction<'_>,
    mapped: &Mapped,
) -> Result<Vec<BackendEvent>, StoreError> {
    let mut events = Vec::new();
    match &mapped.mirror {
        None => {}
        Some(Mirror::Unarchive(flag)) => {
            let changed = chat_state::set_account_flag(tx, chat_state::UNARCHIVE_CHATS, *flag)?;
            // The setting can arrive after the archive records of the same snapshot (both live in `regular_low`, in no
            // order we control): the archives a newer message already made stale go back now (F1).
            if *flag && changed {
                for stale in chat_state::stale_archives(tx)? {
                    chat_state::clear_archive(tx, &stale.chat)?;
                    if stale.stated {
                        events.extend(map::core_event(&stale.chat, Field::Archived, DEFAULT));
                    }
                }
            }
        }
        // The phone's address book: the mirror, and a name for each person of the entry the core knows (B8b, `crate::book`).
        Some(Mirror::Book(change)) => events.extend(crate::book::apply(
            tx,
            change,
            generation(tx, mapped.collection)?,
        )?),
        Some(Mirror::Chat(setting)) => {
            let known = chat_state::core_knows(tx, &setting.chat)?;
            // F1: with the phone's setting at "unarchive", an archive older than a message that counts is a stale record
            // of a chat the phone moved out of the archive by itself: it keeps the default.
            let value = if setting.field == Field::Archived
                && setting.value != DEFAULT
                && refuses_archive(tx, &setting.chat, setting.action_ms)?
            {
                DEFAULT
            } else {
                setting.value
            };
            let written = chat_state::put(
                tx,
                &Put {
                    chat: &setting.chat,
                    field: setting.field,
                    value,
                    action_ms: setting.action_ms,
                    generation: generation(tx, mapped.collection)?,
                    collection: mapped.collection,
                    stated: known,
                },
            )?;
            if setting.field == Field::Pinned {
                // A pin is the full list: restated when this chat's pin changed, or was pinned again (a new place in
                // the order). Unpinning a chat that was not pinned changes nothing.
                if written.changed || (written.retimed && value != DEFAULT) {
                    events.push(BackendEvent::PinnedConversations(chat_state::pinned(tx)?));
                }
            } else if written.changed {
                events.extend(map::core_event(&setting.chat, setting.field, value));
            }
        }
    }
    // The phone cleared or deleted a chat (D14, DUR-10): the records of Kourier's uploads in it hold the media keys of
    // messages the core is just told to remove, and no re-upload request can come for their files. Forgotten in this
    // same zone, as Kourier's own clear forgets them (`send::receipts::confirm_clear`).
    for event in &mapped.events {
        if let BackendEvent::ConversationCleared {
            conv,
            remove,
            before_ms,
            at,
            ..
        } = event
        {
            // A clear by a range time keeps the records of files sent after the range (G4, NEW-4: those messages are
            // still in the chat); a delete of the chat and a clear that names no time forget them all.
            match (remove, before_ms) {
                (false, Some(until)) => crate::send::record::forget_chat_until(tx, conv, *until)?,
                _ => crate::send::record::forget_chat(tx, conv)?,
            };
            // The cut of the chat (F3): a call line of the phone's log after it brings the chat back, one at or below it
            // stays gone. The range's time, else (a clear that names messages only) the action's own.
            chat_state::raise_cut(tx, conv, before_ms.unwrap_or(*at))?;
        }
    }
    events.extend(mapped.events.iter().cloned());
    Ok(events)
}

/// What the phone said about `chat` that the core has not heard, as events, for the zone that has just stated the chat
/// (H5): each unstated, non-default value, then the pin list when one of them was a pin. Marks them stated. Empty for a
/// chat the phone said nothing about (the usual case).
pub(crate) fn settings_of(
    tx: &Transaction<'_>,
    chat: &ConvKey,
) -> Result<Vec<BackendEvent>, StoreError> {
    let waiting = chat_state::to_state(tx, chat)?;
    if waiting.is_empty() {
        return Ok(Vec::new());
    }
    let mut events: Vec<BackendEvent> = Vec::new();
    let mut pin = false;
    for row in &waiting {
        // F1: an archive a message that counts is newer than is stale (the same rule as `apply`): the chat is stated
        // as it is, not archived.
        if row.field == Field::Archived && refuses_archive(tx, chat, row.action_ms)? {
            chat_state::clear_archive(tx, chat)?;
            continue;
        }
        pin |= row.field == Field::Pinned;
        events.extend(map::core_event(chat, row.field, row.value));
    }
    chat_state::mark_stated(tx, chat)?;
    if pin {
        events.push(BackendEvent::PinnedConversations(chat_state::pinned(tx)?));
    }
    Ok(events)
}

/// What a [`reconcile`] did.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Reconciled {
    /// The core events, in order: each reset chat's word (by chat and field), then the full pin list when a pin went.
    pub(crate) events: Vec<BackendEvent>,
    /// Rows put back to the default (a contact of the address book the phone no longer has counts: his row is gone).
    pub(crate) reset: usize,
    /// Rows left as they were because an intent of ours is open on them (M8).
    pub(crate) kept: usize,
}

/// Whether a write of ours (W6b) for this row is waiting to go out or is out and unanswered (M8). W6b has no intents
/// yet: nothing is open. W6b makes this read its table; the reconcile skips the rows it names.
fn has_open_intent(
    _tx: &Transaction<'_>,
    _chat: &ConvKey,
    _field: Field,
) -> Result<bool, StoreError> {
    Ok(false)
}

/// The reconcile of `collection` after the snapshot of `generation` (see the module doc).
pub(crate) fn reconcile(
    tx: &Transaction<'_>,
    collection: &str,
    generation: i64,
) -> Result<Reconciled, StoreError> {
    reconcile_with(tx, collection, generation, &has_open_intent)
}

/// [`reconcile`] with the question "does this row have an open intent?" given (the tests give their own).
pub(crate) fn reconcile_with(
    tx: &Transaction<'_>,
    collection: &str,
    generation: i64,
    intent: &dyn Fn(&Transaction<'_>, &ConvKey, Field) -> Result<bool, StoreError>,
) -> Result<Reconciled, StoreError> {
    let mut done = Reconciled::default();
    let mut pin_went = false;
    for row in chat_state::unseen(tx, collection, generation)? {
        if intent(tx, &row.chat, row.field)? {
            done.kept += 1;
            continue;
        }
        chat_state::reset(tx, &row.chat, row.field, generation)?;
        done.reset += 1;
        if !row.stated {
            // The core does not know this value (or the chat): the mirror alone goes back.
            continue;
        }
        if row.field == Field::Pinned {
            pin_went = true;
        } else {
            done.events
                .extend(map::core_event(&row.chat, row.field, DEFAULT));
        }
    }
    if pin_went {
        done.events
            .push(BackendEvent::PinnedConversations(chat_state::pinned(tx)?));
    }
    // The phone's address book lives in one collection and is mirrored by generation like the chat settings (F19, DUR-6):
    // a contact the snapshot did not mention is gone from the phone, and so is his name from the core.
    if collection == map::CONTACT {
        let (events, gone) = crate::book::reconcile(tx, generation)?;
        done.events.extend(events);
        done.reset += gone;
    }
    Ok(done)
}

/// Whether the phone's setting says a new message brings an archived chat back ("Keep chats archived" is OFF). Never said
/// is no unarchive.
fn unarchives(tx: &Transaction<'_>) -> Result<bool, StoreError> {
    Ok(chat_state::account_flag(tx, chat_state::UNARCHIVE_CHATS)? == Some(true))
}

/// Whether an archive of `chat` dated `archive_ms` is stale and keeps the default: the setting unarchives and a message
/// that counts is STRICTLY newer than the archive (F1, INTEROP-1, DUR-3). An archive carries the time of the last message
/// of its range, so the message that was last when the user archived has the archive's own time: that one is not newer,
/// and the archive holds.
fn refuses_archive(
    tx: &Transaction<'_>,
    chat: &ConvKey,
    archive_ms: i64,
) -> Result<bool, StoreError> {
    if !unarchives(tx)? {
        return Ok(false);
    }
    Ok(chat_state::newest_counted(tx, chat)?.is_some_and(|newest| newest > archive_ms))
}

/// Whether this message brings its chat back out of the archive, by the message alone (the module doc).
fn message_counts(message: &IncomingMessage) -> bool {
    match &message.body {
        Body::System(SystemEvent::Call(_)) => true,
        Body::System(_) | Body::Placeholder(_) => false,
        _ => match message.origin {
            Origin::Remote { .. } => !message.read,
            Origin::OwnOtherDevice { .. } => true,
            // Kourier's own sends do not bring the chat back here: the phone does it for what it sent itself.
            Origin::OwnThisDevice => false,
        },
    }
}

/// [`message_counts`] for the rows of the history push: every one of them says `read: true` (WhatsApp states a chat's
/// unread COUNT only, so the chat carries the count and the rows are read), which says nothing about the message, so
/// "arrives already read" does not exclude a message of somebody else there.
fn history_message_counts(message: &IncomingMessage) -> bool {
    match &message.body {
        Body::System(SystemEvent::Call(_)) => true,
        Body::System(_) | Body::Placeholder(_) => false,
        _ => !matches!(message.origin, Origin::OwnThisDevice),
    }
}

/// Whether `message` is a call line.
fn is_call(message: &IncomingMessage) -> bool {
    matches!(message.body, Body::System(SystemEvent::Call(_)))
}

/// Notes, per chat, the newest message of `events` that counts ([`message_counts`]) in this zone (F1: the time
/// [`refuses_archive`] and [`with_unarchive`] compare an archive with). One write per chat. A CALL LINE is left out
/// (G1, NEW-1): the phone dates an archive by the range's `lastMessageTimestamp`, and calls are on the range's SYSTEM
/// side, so a ring that came after the last text does not make the phone's archive stale. (A call into an archived chat
/// still brings it back, [`with_unarchive`]: that is the A1 rule, kept.)
fn note_counted(tx: &Transaction<'_>, events: &[BackendEvent]) -> Result<(), StoreError> {
    let mut newest: Vec<(&ConvKey, i64)> = Vec::new();
    for event in events {
        if let BackendEvent::Message(message) = event
            && message_counts(message)
            && !is_call(message)
        {
            match newest.iter_mut().find(|(chat, _)| *chat == &message.conv) {
                Some(entry) => entry.1 = entry.1.max(message.sent_at),
                None => newest.push((&message.conv, message.sent_at)),
            }
        }
    }
    for (chat, at) in newest {
        chat_state::note_counted(tx, chat, at)?;
    }
    Ok(())
}

/// `events` with `archived: Some(false)` in front of the first counting message of each archived chat that is NEWER than
/// the chat's archive, when the phone's setting says a new message unarchives (H6, F1); the mirror follows in this zone,
/// stamped with the message's own time. `None` when nothing changed (also when the phone never said the setting: no word
/// is no unarchive). The caller states the chats of `events` FIRST (`features::with_statements`), so a chat first stated
/// in this very zone is archived by then.
pub(crate) fn with_unarchive(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
) -> Result<Option<Vec<BackendEvent>>, StoreError> {
    if !events
        .iter()
        .any(|event| matches!(event, BackendEvent::Message(m) if message_counts(m)))
        || !unarchives(tx)?
    {
        return Ok(None);
    }
    let mut out: Option<Vec<BackendEvent>> = None;
    let mut brought_back: Vec<&ConvKey> = Vec::new();
    for (i, event) in events.iter().enumerate() {
        if let BackendEvent::Message(message) = event
            && message_counts(message)
            && !brought_back.contains(&&message.conv)
            && archived_before(tx, &message.conv, message.sent_at)?
        {
            brought_back.push(&message.conv);
            chat_state::put(
                tx,
                &Put {
                    chat: &message.conv,
                    field: Field::Archived,
                    value: DEFAULT,
                    action_ms: message.sent_at,
                    generation: generation(tx, map::ARCHIVE)?,
                    collection: map::ARCHIVE,
                    stated: true,
                },
            )?;
            let out = out.get_or_insert_with(|| events[..i].to_vec());
            out.extend(map::core_event(&message.conv, Field::Archived, DEFAULT));
        }
        if let Some(out) = &mut out {
            out.push(event.clone());
        }
    }
    Ok(out)
}

/// Whether the core has `chat` archived since a time before `at`: the mirror's value, once told, and its time (the last
/// message of the archive's range) older than `at`. A message older than the archive (the catch-up order) or of the very
/// time of its range brings nothing back.
fn archived_before(tx: &Transaction<'_>, chat: &ConvKey, at: i64) -> Result<bool, StoreError> {
    Ok(chat_state::get(tx, chat, Field::Archived)?
        .is_some_and(|row| row.stated && row.value != DEFAULT && at > row.action_ms))
}

/// The zone of a message row: the newest counting time noted, its chats stated (with what the phone said about them),
/// then the unarchive. What stage 2 and the event writer call for every row that names a chat; `None` when the events
/// stand as they are.
pub(crate) fn prepare(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
) -> Result<Option<Vec<BackendEvent>>, StoreError> {
    // FIRST: a chat first stated in this zone is stated against the time of the messages it rides with.
    note_counted(tx, events)?;
    let stated = crate::features::with_statements(tx, events)?;
    let unarchived = with_unarchive(tx, stated.as_deref().unwrap_or(events))?;
    Ok(unarchived.or(stated))
}

/// The history delivery's zones (F1): notes the newest message of `messages` (all of `conv`) that counts
/// ([`history_message_counts`]) and, when the core holds the chat archived with an archive older than it, the phone's
/// setting has moved the chat out: the mirror follows and the event goes in front of the batch. The history is no
/// unarchive of its own (a push brings nothing back that the phone has not), it only knows what the phone's setting
/// already did. Empty when nothing changed.
pub(crate) fn after_history(
    tx: &Transaction<'_>,
    conv: &ConvKey,
    messages: &[IncomingMessage],
) -> Result<Vec<BackendEvent>, StoreError> {
    let Some(newest) = messages
        .iter()
        // A call line dates nothing (G1, NEW-1): see `note_counted`.
        .filter(|message| history_message_counts(message) && !is_call(message))
        .map(|message| message.sent_at)
        .max()
    else {
        return Ok(Vec::new());
    };
    chat_state::note_counted(tx, conv, newest)?;
    if unarchives(tx)? && archived_before(tx, conv, newest)? {
        chat_state::clear_archive(tx, conv)?;
        return Ok(map::core_event(conv, Field::Archived, DEFAULT)
            .into_iter()
            .collect());
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests;
