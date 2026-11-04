//! The phone's address book (W6a2, chunk B8b; decision D8 of `target/wa/w6-decisions.md`, his answer "~Name, as the phone").
//!
//! WHERE THE NAMES COME FROM: the `contact` mutation of `critical_unblock_low` (a name for one id; a REMOVE drops it) and
//! the history's `InlineContact`s (kind 3). Both are written into the MIRROR (`address_book`, [`crate::store::book`]) as the
//! phone said them, whoever the person is; the contract's field is `ContactPatch.system_name`, kept apart from the push
//! name (`profile_name`, [`crate::names`]) so the display rule can tell the two.
//!
//! WHO GETS A NAME STATED (the scope rule): only a person Kourier has a chat with or who is in one of its groups
//! (`persons_known`: written in the zone that states him as a direct chat's peer, a member of a stated list or the author of
//! a message). The rest of the address book waits in the mirror and is stated in the zone where the person first appears
//! ([`with_names`], [`states`]); the mirror goes with the account's file. A name is stated on a change only
//! (`names_stated`, kind `system`), exactly as the push names are.
//!
//! THE NUMBER: an address-book entry that names the person's number states it with the name: `identifiers` = the number as
//! `+<digits>` and `number_shown: Some(true)` (the phone shows the number of a contact it has saved: Signal's rule for a
//! system contact). Beyond the entry nothing states a number: a person known by a `pn:` id shows it as his label anyway, and
//! a LID-addressed group member's number stays hidden (`None` counts as not shown) because WhatsApp hides it on purpose. When the
//! entry goes, `number_shown: Some(false)` is stated and the name is cleared (or restated from another entry that names
//! the same person).
//!
//! Nothing here logs. A name, an id and a number never leave the zone except in the events it returns; `Debug` of every type
//! here prints nothing of its fields.

use std::collections::HashSet;
use std::fmt;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Transaction;
use kourier_model::{ContactPatch, ConvKey, Identifier, IdentifierKind, Origin, Patch, UserId};
use wacore::store::error::StoreError;
use wacore_binary::Jid;
use waproto::whatsapp as wa;

use crate::map::person_of;
use crate::names::NAME_CAP;
use crate::store::book::{self, Row, Source};
use crate::store::people::{self, NameKind};

/// One contact of the address book as the phone said it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The id the phone indexed the contact under.
    contact: UserId,
    /// The contact's full name, else the first.
    name: String,
    /// The `pn:` form of the person, when the entry names one.
    pn: Option<UserId>,
    /// The `lid:` form of the person, when the entry names one.
    lid: Option<UserId>,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Entry(..)")
    }
}

impl Entry {
    /// The mirror's row of the entry.
    pub(crate) fn row(&self) -> Row {
        Row {
            contact: self.contact.as_str().to_owned(),
            name: self.name.clone(),
            pn: self.pn.as_ref().map(|u| u.as_str().to_owned()),
            lid: self.lid.as_ref().map(|u| u.as_str().to_owned()),
        }
    }
}

/// What the phone said about one contact of its address book.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Change {
    /// The contact has this name (and these ids).
    Put(Entry),
    /// The contact is gone from the address book (a REMOVE, or an entry that names nobody).
    Remove(UserId),
}

impl fmt::Debug for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Put(_) => "Put(..)",
            Self::Remove(_) => "Remove(..)",
        })
    }
}

/// The name `raw` makes: trimmed, not empty, cut to [`NAME_CAP`] characters. Unlike a push name, `"-"` is a name here (the
/// user typed it into his own address book).
pub(crate) fn clean_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    if name.is_empty() {
        return None;
    }
    Some(name.chars().take(NAME_CAP).collect())
}

/// The id `raw` names in the `scheme` form (`pn` or `lid`), if it is one.
fn form(raw: Option<&str>, scheme: &str) -> Option<UserId> {
    let user = person_of(&raw?.parse::<Jid>().ok()?)?;
    (user.scheme() == scheme).then_some(user)
}

/// The full name, else the first.
fn name_of(full: Option<&str>, first: Option<&str>) -> Option<String> {
    full.and_then(clean_name)
        .or_else(|| first.and_then(clean_name))
}

/// What the `contact` mutation of `index` says: its name and the ids it names, or the contact's removal when the mutation
/// names nobody. `None` when the index is no person.
pub(crate) fn change_of_action(
    index: &Jid,
    action: &wa::sync_action_value::ContactAction,
) -> Option<Change> {
    let contact = person_of(index)?;
    let Some(name) = name_of(action.full_name.as_deref(), action.first_name.as_deref()) else {
        return Some(Change::Remove(contact));
    };
    let pn = if contact.scheme() == "pn" {
        Some(contact.clone())
    } else {
        form(action.pn_jid.as_deref(), "pn")
    };
    let lid = if contact.scheme() == "lid" {
        Some(contact.clone())
    } else {
        form(action.lid_jid.as_deref(), "lid")
    };
    Some(Change::Put(Entry {
        contact,
        name,
        pn,
        lid,
    }))
}

/// What the `contact` mutation's REMOVE says: the contact is gone. `None` when the index is no person.
pub(crate) fn change_of_removal(index: &Jid) -> Option<Change> {
    person_of(index).map(Change::Remove)
}

/// What an `InlineContact` of the history says (history kind 3): a named entry under its number, else its LID. `None` for one
/// with no name or no usable id.
pub(crate) fn change_of_inline(contact: &wa::InlineContact) -> Option<Change> {
    let name = name_of(contact.full_name.as_deref(), contact.first_name.as_deref())?;
    let pn = form(contact.pn_jid.as_deref(), "pn");
    let lid = form(contact.lid_jid.as_deref(), "lid");
    let index = pn.clone().or_else(|| lid.clone())?;
    Some(Change::Put(Entry {
        contact: index,
        name,
        pn,
        lid,
    }))
}

/// The forms of the person a row names, once each: the index id, the number, the LID.
fn forms(row: &Row) -> Vec<&str> {
    let mut forms: Vec<&str> = vec![row.contact.as_str()];
    for form in [row.pn.as_deref(), row.lid.as_deref()]
        .into_iter()
        .flatten()
    {
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    forms
}

/// The number a row states: `+<digits>` of its `pn:` form.
fn number_of(row: &Row) -> Option<String> {
    let pn = UserId::parse(row.pn.as_deref()?).ok()?;
    Some(format!("+{}", pn.value()))
}

/// States, for `form` (an id of a person the core knows), what `row` says: the name and the number, when either is not what
/// the core was told. Records what was stated in this zone. `true` when an event was added.
fn state_row(
    tx: &Transaction<'_>,
    form: &str,
    row: &Row,
    events: &mut Vec<BackendEvent>,
) -> Result<bool, StoreError> {
    let Ok(user) = UserId::parse(form) else {
        return Ok(false);
    };
    let name_changed = people::put_name(tx, form, NameKind::System, &row.name)?;
    let number = number_of(row);
    let number_changed = match &number {
        Some(number) => people::put_name(tx, form, NameKind::Number, number)?,
        None => people::drop_name(tx, form, NameKind::Number)?,
    };
    if !name_changed && !number_changed {
        return Ok(false);
    }
    let mut patch = ContactPatch::new(user);
    patch.system_name = Patch::Set(row.name.clone());
    match number {
        Some(value) => {
            patch.identifiers = Some(vec![Identifier {
                kind: IdentifierKind::Phone,
                value,
            }]);
            patch.number_shown = Some(true);
        }
        // An entry that lost its number: the stated number goes out of `identifiers` too, not only out of sight (PRIV-8).
        None if number_changed => {
            patch.identifiers = Some(Vec::new());
            patch.number_shown = Some(false);
        }
        None => {}
    }
    events.push(BackendEvent::ContactUpsert(patch));
    Ok(true)
}

/// The zone of one change of the phone's address book: the mirror (stamped with the collection's `generation`, like
/// `chat_state::put`: the reconcile of a snapshot finds the contacts it did not mention by it), and the statements for the
/// people of the entry the core knows. Empty when nothing is to be told (a person the core does not know waits in the
/// mirror).
pub(crate) fn apply(
    tx: &Transaction<'_>,
    change: &Change,
    generation: i64,
) -> Result<Vec<BackendEvent>, StoreError> {
    apply_from(tx, change, Source::AppState, generation)
}

/// [`apply`] for `source`. The live word (the app state) also takes the history's rows that name the same person: they are
/// older, and one person is one entry (W6a2 PRIV-4).
fn apply_from(
    tx: &Transaction<'_>,
    change: &Change,
    source: Source,
    generation: i64,
) -> Result<Vec<BackendEvent>, StoreError> {
    let mut events = Vec::new();
    match change {
        Change::Put(entry) => {
            let row = entry.row();
            book::put_from(tx, &row, source, generation)?;
            let wanted = forms(&row);
            if source == Source::AppState {
                book::drop_history_naming(tx, &wanted)?;
            }
            for form in book::known_of(tx, &wanted)? {
                state_row(tx, &form, &row, &mut events)?;
            }
        }
        Change::Remove(contact) => {
            let Some(row) = book::take(tx, contact.as_str())? else {
                return Ok(events);
            };
            if source == Source::AppState {
                book::drop_history_naming(tx, &forms(&row))?;
            }
            for form in book::known_of(tx, &forms(&row))? {
                if people::name_of(tx, &form, NameKind::System)?.is_none() {
                    continue;
                }
                // Another entry may name the same person: he keeps a name, from it.
                if let Some(other) = book::of(tx, &form)? {
                    state_row(tx, &form, &other, &mut events)?;
                    continue;
                }
                people::drop_name(tx, &form, NameKind::System)?;
                let had_number = people::drop_name(tx, &form, NameKind::Number)?;
                let Ok(user) = UserId::parse(&form) else {
                    continue;
                };
                let mut patch = ContactPatch::new(user);
                patch.system_name = Patch::Clear;
                if had_number {
                    // The number leaves with the name: core.db keeps none of a contact the phone no longer has (PRIV-8).
                    patch.identifiers = Some(Vec::new());
                    patch.number_shown = Some(false);
                }
                events.push(BackendEvent::ContactUpsert(patch));
            }
        }
    }
    Ok(events)
}

/// The zone of one entry the phone's HISTORY kept (kind 3): the same as [`apply`], except that a person the mirror already
/// holds is left as he is. The app state's `contact` mutation is the phone's live word and fresher than an old history push,
/// which is taken up long after it; it must never win over it. "The same person" is any row that is indexed under or names
/// the entry's number OR its LID, not only the same index (the app state may index a contact by its LID where the history
/// keeps it by its number: W6a2 PRIV-4); the row is kept apart by its origin so a later live word can take it back.
pub(crate) fn apply_older(
    tx: &Transaction<'_>,
    change: &Change,
) -> Result<Vec<BackendEvent>, StoreError> {
    let named: Vec<String> = match change {
        Change::Put(entry) => forms(&entry.row()).into_iter().map(str::to_owned).collect(),
        Change::Remove(contact) => vec![contact.as_str().to_owned()],
    };
    let named: Vec<&str> = named.iter().map(String::as_str).collect();
    if book::names_any(tx, &named)? {
        return Ok(Vec::new());
    }
    // A history row has no generation: it is not the collection's (the reconcile leaves it alone).
    apply_from(tx, change, Source::History, 0)
}

/// The reconcile of the address book after the snapshot of `generation` of its collection (W6a2 F19, DUR-6; the zone is
/// `appstate::zone::reconcile`, which decides WHETHER: the replay mentioned a row of the collection): every contact the
/// app state said that the replay left on an older generation is what the phone no longer has, and goes the way a REMOVE
/// of it goes ([`Change::Remove`]: the name taken back from the people the core knows, another entry of the same person
/// keeping him a name). The events, in order, and how many contacts went.
pub(crate) fn reconcile(
    tx: &Transaction<'_>,
    generation: i64,
) -> Result<(Vec<BackendEvent>, usize), StoreError> {
    let mut events = Vec::new();
    let mut gone = 0;
    for contact in book::unseen(tx, generation)? {
        // A row is written from a `UserId` and read back as one.
        let Ok(contact) = UserId::parse(&contact) else {
            continue;
        };
        events.extend(apply(tx, &Change::Remove(contact), generation)?);
        gone += 1;
    }
    Ok((events, gone))
}

/// The zone of the core's `RefreshContact` for `user` (no network): states again what this backend holds of him, the
/// self-chosen name as the core was told it and, when the core knows him, the address-book name and number from the mirror.
/// The statements are taken back first, so the core hears them even if it heard them before. Empty when there is nothing.
pub(crate) fn restate(
    tx: &Transaction<'_>,
    user: &UserId,
) -> Result<Vec<BackendEvent>, StoreError> {
    let form = user.as_str();
    let mut events = Vec::new();
    if let Some(name) = people::name_of(tx, form, NameKind::Profile)? {
        // A name kept by a `Rebuild` is told now: it is no longer owed.
        people::put_name(tx, form, NameKind::Profile, &name)?;
        events.push(crate::names::push_name_event(user.clone(), name));
    }
    people::drop_name(tx, form, NameKind::System)?;
    people::drop_name(tx, form, NameKind::Number)?;
    if !book::known_of(tx, &[form])?.is_empty()
        && let Some(row) = book::of(tx, form)?
    {
        state_row(tx, form, &row, &mut events)?;
    }
    Ok(events)
}

/// The people `events` name as a direct chat's peer, a member of a list or a message's author, once each, in order.
pub(crate) fn persons(events: &[BackendEvent]) -> Vec<UserId> {
    let mut seen: HashSet<UserId> = HashSet::new();
    let mut out = Vec::new();
    let mut add = |user: &UserId| {
        if seen.insert(user.clone()) {
            out.push(user.clone());
        }
    };
    for event in events {
        match event {
            BackendEvent::Message(message) => {
                if let ConvKey::Direct(user) = &message.conv {
                    add(user);
                }
                if let Origin::Remote { author, .. } = &message.origin {
                    add(author);
                }
            }
            BackendEvent::ConversationUpsert(patch) => {
                if let ConvKey::Direct(user) = &patch.key {
                    add(user);
                }
                if let Some(members) = &patch.members {
                    for member in &members.upsert {
                        add(&member.user);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Makes each of `people` known to the core and states what he is owed: the pairs and the push name a history chunk named of
/// him while he was a stranger ([`crate::strangers::release`]), and his address-book name (and number) if the mirror has
/// one the core was not told. For the zone that states a chat's peer. The merges come first, then the names. OUR OWN ids are
/// nobody's contact: they are never made known and nothing is stated for them (W6a2 F13, PRIV-7), whatever the phone's
/// address book or a chunk holds under them.
pub(crate) fn states(
    tx: &Transaction<'_>,
    people: &[UserId],
) -> Result<Vec<BackendEvent>, StoreError> {
    let mut merges = Vec::new();
    let mut events = Vec::new();
    // Our own ids are read from the stored device, once, and only for a person not known yet (every other one was let in).
    let mut own: Option<Vec<String>> = None;
    for user in people {
        if book::known_of(tx, &[user.as_str()])?.is_empty() {
            let own = match &own {
                Some(own) => own,
                None => own.insert(crate::store::own_forms(tx)?),
            };
            if own.iter().any(|ours| ours == user.as_str()) {
                continue;
            }
            book::know(tx, user.as_str())?;
        }
        for event in crate::strangers::release(tx, user)? {
            match event {
                BackendEvent::IdentityMerged { .. } => merges.push(event),
                other => events.push(other),
            }
        }
        // A push name the old core was told and a `Rebuild` kept: the new core hears it once, now (F21, DUR-12).
        if let Some(name) = people::unstated_profile(tx, user.as_str())? {
            events.push(crate::names::push_name_event(user.clone(), name));
        }
        if let Some(row) = book::of(tx, user.as_str())? {
            state_row(tx, user.as_str(), &row, &mut events)?;
        }
    }
    merges.extend(events);
    Ok(merges)
}

/// `events` with the address-book names of the people they name, in the zone that writes them: the people become known to the
/// core and their names the mirror holds are stated, after the leading merges (a name is for the person a merge makes) and
/// before everything else. `None` when nothing was to be added (the caller writes `events` as they are).
pub(crate) fn with_names(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
) -> Result<Option<Vec<BackendEvent>>, StoreError> {
    let named = states(tx, &persons(events))?;
    if named.is_empty() {
        return Ok(None);
    }
    let at = crate::names::place(events);
    let mut out = Vec::with_capacity(events.len() + named.len());
    out.extend_from_slice(&events[..at]);
    out.extend(named);
    out.extend_from_slice(&events[at..]);
    Ok(Some(out))
}

#[cfg(test)]
mod tests;
