//! The people a history chunk named that the core does not know (W6a2 fix F11, PRIV-5: the lead's call "N25 everywhere").
//!
//! A chunk's `phoneNumberToLidMappings` and `pushnames` ([`crate::history::rest::people`]) name whoever the phone had in its
//! mind, mostly people Kourier has no chat or group with. D8's scope rule (and the merge sweep's, N25) keeps those out of
//! core.db: a number of a stranger is an alias row, and a stranger's self-chosen name is text of someone Kourier knows
//! nothing else of. So the chunk's people row is cut in the zone that writes it ([`scope`]):
//! - a pair of which the core knows one person (`persons_known`) and the push name of a person it knows are stated, as before;
//! - the rest is KEPT in backend.db (`kept_pairs`, `kept_names`, [`crate::store::strangers`]) and stated ONCE by the zone
//!   where the person first becomes known ([`release`], called by [`crate::book::states`] for every person a zone names), then
//!   forgotten from there.
//!
//! A kept pair is released only if it crosses no pair the core was told already (a merge is final: a second pair around a
//! number or a LID would join two people); a kept name is released only if neither the live path nor an earlier statement
//! gave the person a push name (the live name is newer than a snapshot the phone took before the delivery). Every pair a
//! chunk states, now or later, is recorded in `merges_stated` in the same zone ([`crate::merges::record`]), so the sweep never
//! states a second pair around the number (PRIV-6).
//!
//! Nothing here logs. An id and a name never leave the zone except in the events it returns.

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Transaction;
use kourier_model::{Patch, UserId};
use wacore::store::error::StoreError;

use crate::history::named;
use crate::store::merges as rows;
use crate::store::people::{self, NameKind};
use crate::store::{book, strangers};

/// The events of a chunk's people row that the core may be told now: a pair of which one person is known, the push name of a
/// person who is known, and every event of another kind. The rest is kept for the day the person is known. A pair that
/// would cross one the core was told is dropped (a merge is final). The caller records the pairs that stay
/// ([`crate::merges::record`]) once the contract check kept them.
pub(crate) fn scope(
    tx: &Transaction<'_>,
    events: Vec<BackendEvent>,
) -> Result<Vec<BackendEvent>, StoreError> {
    let mut now = Vec::with_capacity(events.len());
    for event in events {
        match &event {
            BackendEvent::IdentityMerged { from, into }
                if from.scheme() == "pn" && into.scheme() == "lid" =>
            {
                if rows::either_known(tx, from.as_str(), into.as_str())? {
                    if !rows::crosses_a_stated_pair(tx, from.as_str(), into.as_str())? {
                        now.push(event);
                    }
                } else {
                    strangers::keep_pair(tx, from.as_str(), into.as_str())?;
                }
            }
            BackendEvent::ContactUpsert(patch) if matches!(patch.profile_name, Patch::Set(_)) => {
                let Patch::Set(name) = &patch.profile_name else {
                    continue;
                };
                if book::known_of(tx, &[patch.user.as_str()])?.is_empty() {
                    strangers::keep_name(tx, patch.user.as_str(), name)?;
                } else {
                    now.push(event);
                }
            }
            _ => now.push(event),
        }
    }
    Ok(now)
}

/// What a person who has just become known (or is known and named by a zone again) is owed of the chunks: the pairs kept
/// for him, as `IdentityMerged` (merges first, as the zones order them), then his kept push name. The rows are taken out of
/// the kept tables in this zone and the statements recorded in it (`merges_stated`, `names_stated`), so a kill leaves both
/// or neither. Empty when nothing is kept.
pub(crate) fn release(
    tx: &Transaction<'_>,
    user: &UserId,
) -> Result<Vec<BackendEvent>, StoreError> {
    let person = user.as_str();
    let mut events = Vec::new();
    for (pn, lid) in strangers::take_pairs(tx, person)? {
        if rows::crosses_a_stated_pair(tx, &pn, &lid)? || !rows::put_stated(tx, &pn, &lid)? {
            continue;
        }
        if let (Ok(from), Ok(into)) = (UserId::parse(&pn), UserId::parse(&lid)) {
            events.push(BackendEvent::IdentityMerged { from, into });
        }
    }
    if let Some(name) = strangers::take_name(tx, person)? {
        // The live path's name (or one the core was told before) is newer than a chunk the phone made earlier.
        let told = people::name_of(tx, person, NameKind::Profile)?.is_some()
            || !named::named(tx, &[person])?.is_empty();
        if !told && people::put_name(tx, person, NameKind::Profile, &name)? {
            events.push(crate::names::push_name_event(user.clone(), name));
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests;
