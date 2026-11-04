//! The zone half of a group's notice (W6a2, chunk B5; decisions D9, D10, M4 and M5 of `target/wa/w6-decisions.md`): what the
//! acts of a notice ([`crate::notice::Act`]) come to against the group's row, in the zone that writes the notice. The row
//! (`group_state`, [`crate::groups::State`]) is what the core was told, so a notice is compared with it and the row follows
//! what the zone states.
//!
//! One notice, one `ConversationUpsert` of the group (title, member delta, features) in front of the line, and the row; a
//! notice that changes nothing states nothing. Rules:
//! - the TITLE only when it is not older than the one the core was told (M5, scout T6); the row keeps the newer time;
//! - a MEMBER DELTA only when the core holds the group's whole list (`members_stated`); otherwise the group is marked for a
//!   fetch, because a delta on no list would show a partial one. A delta is always stated WITH the features (M4: the zone
//!   that states a group's members always states its features too, so the core never holds an admin it would offer to
//!   delete for, with features that say nothing of it);
//! - a RULE or our own place only when the row holds rules (a fetch answered); otherwise the group is marked for a fetch, as
//!   one bit stated alone would claim the others off. The features are made again from the row ([`groups::features`]), so
//!   `group_info` and `cannot_send` follow `Announce`, `Locked`, `MemberAddMode`, a promote or a demote of us, and a
//!   removal;
//! - a notice that says we are out of the group (`Gone`) or that it is over (`Deleted`) finishes the group's parked admin
//!   deletes when its list was never stated, in the same zone, with no event (fix F5);
//! - the mark "fetch this group" is the row's `dirty_at` ([`people::mark_dirty`]); the fetch policy (chunk B6) asks under
//!   its bucket, never before `Online`, and the answer's zone clears it.
//!
//! The ANSWER of a fetch has its own zone half, [`fetched`] (chunk B6): the batch of the start and the single query of a
//! group the policy ([`crate::groups::fetch`]) asked for.
//!
//! Nothing here logs; ids, names and titles never leave the zone except as the events it returns.

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Transaction;
use kourier_model::{ConvKey, ConversationPatch, MemberList, Patch};
use wacore::store::error::StoreError;
use whatsapp_rust::GroupMetadataResult;

use crate::groups::{self, Fetch, Rules};
use crate::map::OwnIds;
use crate::notice::{Act, Noticed};
use crate::send::started;
use crate::store::{chat_state, people};

/// The events of `noticed` with the zone's own upsert in front of the line (or last when there is no line); the row follows.
/// `now_ms` is the time of the mark "fetch this group".
pub(crate) fn apply(
    tx: &Transaction<'_>,
    noticed: &Noticed,
    now_ms: i64,
) -> Result<Vec<BackendEvent>, StoreError> {
    // The actor's name only when the core was not told it (W6a2, B8a, D8).
    let mut events = crate::names::keep_changed(tx, noticed.events.clone())?;
    if noticed.acts.is_empty() {
        return Ok(events);
    }
    let conv = &noticed.conv;
    let before = people::group(tx, conv)?;
    let mut state = before.clone();
    let mut patch = ConversationPatch::new(conv.clone());
    let mut members: Option<MemberList> = None;
    let mut restate = false;
    let mut fetch = false;
    // The notice says we are out of the group, or the group is over (fix F5).
    let mut over = false;
    for act in &noticed.acts {
        match act {
            Act::Title { title, at_s } => {
                if groups::title_is_current(state.subject_time, Some(*at_s)) {
                    patch.title = Patch::Set(title.clone());
                    state.subject_time = Some(state.subject_time.map_or(*at_s, |s| s.max(*at_s)));
                }
            }
            Act::Members { upsert, remove } => {
                let list = members.get_or_insert_with(MemberList::default);
                list.upsert.extend(upsert.iter().cloned());
                list.remove.extend(remove.iter().cloned());
            }
            Act::Rule(change) => match state.rules {
                Some(rules) => {
                    let after = rules.with(*change);
                    if after != rules {
                        state.rules = Some(after);
                        restate = true;
                    }
                }
                None => fetch = true,
            },
            Act::Deleted => {
                over = true;
                let rules = state.rules.unwrap_or_default();
                let ended = Rules {
                    suspended: true,
                    ..rules
                };
                if !state.not_member || state.rules != Some(ended) {
                    state.not_member = true;
                    state.rules = Some(ended);
                    restate = true;
                }
            }
            Act::Gone => {
                over = true;
                if !state.not_member {
                    state.not_member = true;
                    restate = true;
                }
            }
            Act::Back => {
                if state.not_member {
                    // What was true when we left says nothing about the group now: nothing is claimed until it is read.
                    state.not_member = false;
                    state.rules = None;
                    state.members_stated = false;
                    restate = true;
                }
                // Whether we joined a group never fetched or came back to one, we know nothing of it yet.
                fetch = true;
            }
            Act::Fetch => fetch = true,
        }
    }
    // The delta, against the row AS IT IS after the acts: `Back` may have cleared the flag in this very notice, so the
    // delta of a re-add is dropped with the rest and the fetch has the list.
    if let Some(list) = members.filter(|list| !list.upsert.is_empty() || !list.remove.is_empty()) {
        if state.members_stated {
            patch.members = Some(list);
            restate = true;
        } else {
            fetch = true;
        }
    }
    // A group we left (or that was deleted) answers no fetch for a list never stated: the admin deletes that wait for it can
    // never be judged and are finished now, with no event and their content wiped, as the batch zone does for a refusal
    // (M3). Without this they sat in `backend.db` until their 45 days (fix F5, PROTOCOL-9 + DUR-10).
    if over && state.not_member && !state.members_stated {
        people::end_waiting(tx, conv)?;
    }
    if restate {
        patch.features = Some(groups::features(conv, &state));
    }
    if state != before {
        people::put_group(tx, conv, &state)?;
    }
    if fetch {
        people::mark_dirty(tx, conv, now_ms)?;
    }
    if patch != ConversationPatch::new(conv.clone()) {
        let place = events
            .iter()
            .position(|event| matches!(event, BackendEvent::Message(_)))
            .unwrap_or(events.len());
        events.insert(place, BackendEvent::ConversationUpsert(patch));
    }
    Ok(events)
}

/// What the answers of one fetch came to, in numbers (the log's, never an id).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Groups stated whole.
    pub(crate) found: usize,
    /// Groups the server named and gave nothing else (the row says: one single query per start).
    pub(crate) truncated: usize,
    /// Groups we are not in (`Forbidden`, `NotFound`).
    pub(crate) not_member: usize,
    /// Entries about something that is no group, or a kind this build does not read.
    pub(crate) skipped: usize,
    /// Admin deletes that waited for a member list this answer stated: let go (W6a2, D11).
    pub(crate) released: usize,
    /// Admin deletes finished with no event because this answer says we are not in their group (W6a2, M3).
    pub(crate) ended: usize,
    /// Subgroups whose community the core did not know: their parent was left out and waits in the row (Q3).
    pub(crate) parents_waiting: usize,
    /// Subgroups told their community because this zone stated it first (Q3).
    pub(crate) parents_released: usize,
}

impl Counts {
    /// Whether anything was stated or recorded.
    pub(crate) fn any(self) -> bool {
        self.found + self.truncated + self.not_member + self.released + self.ended > 0
    }
}

/// What the ANSWERS of a fetch say, as events and rows (W6a2, B6; D9, D10, M4, M5, scout T3-T5): each entry is mapped by
/// [`groups::map`] against the group's row, the row follows, and the events go out in the order of the answer.
///
/// - an entry that is answered whole (`Found`) or says we are not in the group clears the group's "fetch me" mark;
///   a `Truncated` one does NOT (it proves nothing new: the single query that follows clears the mark), and a group the
///   answer does not name keeps its row and its mark untouched (scout T3: absence is no proof);
/// - a refusal ([`GroupMetadataResult::Forbidden`], `NotFound`) of a group the backend never stated to the core states
///   nothing: it would only make a title-less chat of a group nobody here has seen;
/// - the zone that states a group's WHOLE member list lets that group's waiting admin deletes go (stage 2 maps them from now
///   on: D11), in the same transaction; an answer that says we are not in a group whose list was never stated finishes them
///   with no event (M3);
/// - the same group twice in one answer is two steps against the row, in order.
///
/// - a mark made AFTER the ask (`asked_at_ms`) is not covered by the answer (it was computed before the notice) and stays.
///
/// `now_ms` is the time of the answer: the timer's version and the row's `fetched_at`. The caller (the event writer's
/// zone) treats each group's events like any that name a chat ([`crate::appstate::zone::prepare`]) and appends them as ONE
/// `core_outbox` row of their own, all in the same transaction (fix F22, DUR-13): a group's whole member list and
/// description is a big statement, and a single event the core refuses must quarantine that group's row, not the whole
/// batch's. This returns one vector of events per group that made any, in the order of the answer.
pub(crate) fn fetched_by_group(
    tx: &Transaction<'_>,
    answers: &[GroupMetadataResult],
    own: &OwnIds,
    asked_at_ms: i64,
    now_ms: i64,
) -> Result<(Vec<Vec<BackendEvent>>, Counts), StoreError> {
    let fetch = Fetch { own, at_ms: now_ms };
    let mut rows: Vec<Vec<BackendEvent>> = Vec::new();
    let mut counts = Counts::default();
    // The groups this answer has stated whole so far: a subgroup's community stated BEFORE it in this zone is known.
    let mut told: Vec<ConvKey> = Vec::new();
    for answer in answers {
        let Some(conv) = groups::conv_of(answer) else {
            counts.skipped += 1;
            continue;
        };
        let stored = people::group(tx, &conv)?;
        let Some(mut outcome) = groups::map(answer, &stored, &fetch) else {
            counts.skipped += 1;
            continue;
        };
        // This group's events: one row of its own (F22).
        let mut events = Vec::new();
        // A subgroup names its community only when the core has that chat (Kourier stated it) or this answer stated it
        // first; else the parent waits in the row (the core would make a bare, title-less chat of it).
        if let Some(parent) = outcome.state.parent.clone()
            && !told.contains(&parent)
            && !chat_state::core_knows(tx, &parent)?
        {
            groups::withhold_parent(&mut outcome);
            counts.parents_waiting += 1;
        }
        people::put_group(tx, &outcome.conv, &outcome.state)?;
        match answer {
            GroupMetadataResult::Truncated { .. } => counts.truncated += 1,
            GroupMetadataResult::Found(_) => {
                counts.found += 1;
                people::clear_dirty(tx, &outcome.conv, asked_at_ms)?;
                if outcome.state.members_stated {
                    counts.released += people::release_waiting(tx, &outcome.conv)?;
                }
                events.extend(outcome.events);
                // This zone states the group: the subgroups that waited for it are told their community now.
                for waiting in people::subgroups_waiting_for(tx, &outcome.conv)? {
                    let mut patch = ConversationPatch::new(waiting.clone());
                    patch.parent = Patch::Set(outcome.conv.clone());
                    events.push(BackendEvent::ConversationUpsert(patch));
                    people::mark_parent_stated(tx, &waiting)?;
                    counts.parents_released += 1;
                }
                told.push(outcome.conv);
            }
            _ => {
                counts.not_member += 1;
                people::clear_dirty(tx, &outcome.conv, asked_at_ms)?;
                if !outcome.state.members_stated {
                    counts.ended += people::end_waiting(tx, &outcome.conv)?;
                }
                // A group this account never stated is not one the core has: nothing is said of it.
                if started::was_stated(tx, &outcome.conv)? {
                    events.extend(outcome.events);
                }
            }
        }
        if !events.is_empty() {
            rows.push(events);
        }
    }
    Ok((rows, counts))
}

/// [`fetched_by_group`] with every group's events in one vector, in order: what the tests of the answers' content read.
#[cfg(test)]
pub(crate) fn fetched(
    tx: &Transaction<'_>,
    answers: &[GroupMetadataResult],
    own: &OwnIds,
    asked_at_ms: i64,
    now_ms: i64,
) -> Result<(Vec<BackendEvent>, Counts), StoreError> {
    let (rows, counts) = fetched_by_group(tx, answers, own, asked_at_ms, now_ms)?;
    Ok((rows.concat(), counts))
}

#[cfg(test)]
mod tests;
