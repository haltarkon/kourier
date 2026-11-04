//! The history map, the rest of a chunk (W4, C10): what a `HistorySync` holds besides its chats. PURE.
//!
//! The streamed chunk hands the feeder its chats one by one and everything else as the `remainder`
//! (`HistorySyncStream`, decoded by the caller into a [`wa::HistorySync`] with no chats). This module maps
//! that rest:
//! - [`people`]: the events that go FIRST in a chunk, before the batches of any of its chats: the pairs of
//!   the chunk's own `phoneNumberToLidMappings` as `IdentityMerged { from: number, into: lid }` (vouched by
//!   our phone; never one that names an id of ours), then one `ContactUpsert` per push name (the name a
//!   person calls himself; never ours, which is the account's own name, not a contact). The feeder cuts the
//!   list in order into rows of at most [`MAX_PEOPLE_PER_ROW`], so the pairs always come before the names. The delivery
//!   zone states them only for the people the core knows and keeps the rest for the day it knows them (`crate::strangers`,
//!   W6a2 F11).
//! - [`kept`]: the parts that are no message and that this build shows nowhere yet, kept for the step that
//!   will (`history_deferred`, O3): the call-log records (taken up as quiet call lines once the push is over, W6a1 D13: `deliver::take_up_calls`), the groups' former members, the
//!   phone's address-book names and its settings. DROPPED ON PURPOSE: the statuses (let go, as live and as
//!   his ROUND 4 answer says); the recent stickers (file pointers with keys, no feature); and every secret,
//!   key or internal state (`threadIdUserSecret`, `shareableChatIdentifierEncryptionKey`, `nctSalt` (upstream
//!   uses it itself), `companionMetaNonce`, the AI wait list, the accounts): Kourier keeps no key it does
//!   not use.

use kourier_backend_api::BackendEvent;
use kourier_model::{ContactPatch, Patch, UserId};
use wacore_binary::Jid;
use waproto::buffa::Message as _;
use waproto::whatsapp as wa;

use crate::map::{Ctx, person_of, vouched_pair};

/// Most events in one row of people (the pairs first, then the names): the delivery row's bound.
pub(crate) const MAX_PEOPLE_PER_ROW: usize = 200;

/// The events of the chunk's people, in the order they go into rows: all the pairs, then all the names.
/// A person named twice keeps the last name given; a pair stated twice is stated once.
pub(crate) fn people(rest: &wa::HistorySync, ctx: &Ctx) -> Vec<BackendEvent> {
    let mut events: Vec<BackendEvent> = Vec::new();

    let mut pairs: Vec<(UserId, UserId)> = Vec::new();
    for mapping in &rest.phone_number_to_lid_mappings {
        let (Some(pn), Some(lid)) = (mapping.pn_jid.as_deref(), mapping.lid_jid.as_deref()) else {
            continue;
        };
        let (Ok(pn), Ok(lid)) = (pn.parse::<Jid>(), lid.parse::<Jid>()) else {
            continue;
        };
        if let Some(pair) = vouched_pair(&pn, &lid, &ctx.own)
            && !pairs.contains(&pair)
        {
            pairs.push(pair);
        }
    }
    events.extend(
        pairs
            .into_iter()
            .map(|(from, into)| BackendEvent::IdentityMerged { from, into }),
    );

    let mut names: Vec<(UserId, String)> = Vec::new();
    for entry in &rest.pushnames {
        let Some(user) = entry
            .id
            .as_deref()
            .and_then(|id| id.parse::<Jid>().ok())
            .and_then(|jid| person_of(&jid))
        else {
            continue;
        };
        // The one rule of what a push name is: not blank, not the placeholder "-", cut (`names::clean`, W6a2).
        let Some(name) = crate::names::clean(entry.pushname.as_deref().unwrap_or_default()) else {
            continue;
        };
        if ctx.own.is_own(&user) {
            continue;
        }
        match names.iter_mut().find(|(known, _)| *known == user) {
            Some(slot) => slot.1 = name,
            None => names.push((user, name)),
        }
    }
    events.extend(names.into_iter().map(|(user, name)| {
        let mut patch = ContactPatch::new(user);
        patch.profile_name = Patch::Set(name);
        BackendEvent::ContactUpsert(patch)
    }));
    events
}

/// What a kept part is, as `history_deferred.kind` stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Kind {
    /// A `WebMessageInfo` this build cannot show yet: delivered later as a silent history row.
    Message = 0,
    /// A `CallLogRecord`: taken up once the push is over (W6a1, `deliver::take_up_calls`) and kept, marked.
    Call = 1,
    /// A group's `PastParticipants`.
    PastParticipants = 2,
    /// An `InlineContact`: a name from the phone's address book.
    Contact = 3,
    /// The phone's `GlobalSettings`.
    Settings = 4,
}

/// A part of a chunk kept for a later build: what it is, the chat it belongs to (`""` for none) and its
/// protobuf bytes. Never delivered as a message unless it is [`Kind::Message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) kind: Kind,
    pub(crate) conv: String,
    pub(crate) item: Vec<u8>,
}

/// The parts of the rest that are kept, in a fixed order (calls, former members, contacts, settings), so the
/// same chunk always numbers them the same.
pub(crate) fn kept(rest: &wa::HistorySync) -> Vec<Part> {
    let mut parts = Vec::new();
    for record in &rest.call_log_records {
        parts.push(Part {
            kind: Kind::Call,
            conv: record.group_jid.clone().unwrap_or_default(),
            item: record.encode_to_vec(),
        });
    }
    for group in &rest.past_participants {
        parts.push(Part {
            kind: Kind::PastParticipants,
            conv: group.group_jid.clone().unwrap_or_default(),
            item: group.encode_to_vec(),
        });
    }
    for contact in &rest.inline_contacts {
        parts.push(Part {
            kind: Kind::Contact,
            conv: String::new(),
            item: contact.encode_to_vec(),
        });
    }
    if let Some(settings) = rest.global_settings.as_option() {
        parts.push(Part {
            kind: Kind::Settings,
            conv: String::new(),
            item: settings.encode_to_vec(),
        });
    }
    parts
}

#[cfg(test)]
mod tests;
