//! The names people call themselves (W6a2, chunk B8a; decision D8 of `target/wa/w6-decisions.md`).
//!
//! A PUSH NAME is the name a person chose for himself; the contract's field is `ContactPatch.profile_name`, kept apart from
//! `system_name` (the phone's address book, B8b) so the display rule can tell the two. Three places carry one:
//! - a live message (`InboxMeta.push_name`, the stanza's `notify`): the map names the sender in `Mapped::push_name`, and
//!   stage 2's zone states it ([`state_push_name`]) only when it differs from what the core was told (`names_stated`, kind
//!   `profile`): a chatty person's name rides once, not on every message;
//! - a group notification (`GroupUpdate.notify`: the actor's name), stated by the group zone the same way;
//! - the phone's history (`HistorySync.pushnames`): delivered by the history path with its own older-than-live rule
//!   (`history::named`); it passes [`clean`] too.
//!
//! [`clean`] is the ONE rule for what a push name is: trimmed, not empty, not the placeholder `"-"` WhatsApp writes when a
//! person has no name (it would otherwise name a hundred strangers "-"), cut to [`NAME_CAP`] characters. Our own name is
//! never stated here (a message from another device of ours, or from one of our ids): the account's name is not a contact.
//!
//! Nothing here logs; a name never leaves the zone except as the event it returns, and the debug lines count.

use std::fmt;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Transaction;
use kourier_model::{ContactPatch, Patch, UserId};
use wacore::store::error::StoreError;

use crate::store::people;

/// A push name is cut to this many characters (WhatsApp's own limit is 25; the bound keeps a hostile message or chunk from
/// putting a page of text into a contact).
pub(crate) const NAME_CAP: usize = 128;

/// What the network writes into a push name field when the person has none.
const PLACEHOLDER: &str = "-";

/// A person and the name he calls himself, as the map found them in a message. `Debug` prints nothing of either: an id and a name
/// are never in a log, and the map's outcome is formatted by tests and by panics.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Named {
    pub(crate) user: UserId,
    pub(crate) name: String,
}

impl fmt::Debug for Named {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Named(..)")
    }
}

/// The push name `raw` says, or `None` when it says no name (empty, blank or the placeholder `"-"`). Pure.
pub(crate) fn clean(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() || name == PLACEHOLDER {
        return None;
    }
    Some(name.chars().take(NAME_CAP).collect())
}

/// The event that says `user` calls himself `name`.
pub(crate) fn push_name_event(user: UserId, name: String) -> BackendEvent {
    let mut patch = ContactPatch::new(user);
    patch.profile_name = Patch::Set(name);
    BackendEvent::ContactUpsert(patch)
}

/// Where a person's name goes among the events of one zone: after the leading `IdentityMerged` events (a merge is final
/// and the name is for the person it makes), in front of everything else.
pub(crate) fn place(events: &[BackendEvent]) -> usize {
    events
        .iter()
        .position(|event| !matches!(event, BackendEvent::IdentityMerged { .. }))
        .unwrap_or(events.len())
}

/// States `name` for `user` in `events` when the core was not told it already, and records what was told, in THIS zone (a
/// kill leaves the event and the record, or neither). `true` when the name was added.
pub(crate) fn state_push_name(
    tx: &Transaction<'_>,
    events: &mut Vec<BackendEvent>,
    user: &UserId,
    name: &str,
) -> Result<bool, StoreError> {
    if !people::put_name(tx, user.as_str(), people::NameKind::Profile, name)? {
        return Ok(false);
    }
    let at = place(events);
    events.insert(at, push_name_event(user.clone(), name.to_owned()));
    Ok(true)
}

/// The contact upserts of `events` that set a push name, kept only when the core was not told that name already (and
/// recorded as told, in THIS zone); every other event stays. For a pure map that names a person in its events (the group
/// notice's actor, `GroupUpdate.notify`) and leaves the comparison to the zone.
pub(crate) fn keep_changed(
    tx: &Transaction<'_>,
    events: Vec<BackendEvent>,
) -> Result<Vec<BackendEvent>, StoreError> {
    let mut kept = Vec::with_capacity(events.len());
    for event in events {
        if let BackendEvent::ContactUpsert(patch) = &event
            && let Patch::Set(name) = &patch.profile_name
            && !people::put_name(tx, patch.user.as_str(), people::NameKind::Profile, name)?
        {
            continue;
        }
        kept.push(event);
    }
    Ok(kept)
}

#[cfg(test)]
mod tests;
