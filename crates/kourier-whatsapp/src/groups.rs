//! The group map (W6a2, chunk B4; decisions D9, D10, M4 and M5 of `target/wa/w6-decisions.md`): what the server's
//! answer about a group says, in the core's terms. PURE: no I/O, no clock, no logging; the zone that writes the events
//! and the row (chunk B6) is the caller's.
//!
//! WHAT AN ANSWER IS (scout T3-T5, `target/wa/w6/scout-groups.md`):
//! - `Found`: the group's WHOLE state at the time of the fetch: one `ConversationUpsert` carrying the title (only when
//!   its `subject_time` is not older than the one the core was told, M5), the description, the member list as a FULL
//!   snapshot (a super admin is an `Admin`: the contract has no owner role), the community parent, the disappearing
//!   timer with the fetch's time as its version (the metadata carries none, T7) and the group's FEATURES, in the same
//!   patch (M4: the members and the features are never stated apart, so the core never holds an admin it would offer
//!   to delete for, with features that say nothing of it).
//! - `Truncated`: the server named the group and its size and gave nothing else (T5). Nothing is stated: the member
//!   list the core holds stays (never an empty one) and the row is marked, so the group gets ONE single query per
//!   start (M1).
//! - `Forbidden` / `NotFound` (and a single query's 403 and 404): we are not in the group (we left, were removed, or it
//!   is gone). The chat is stated `NotMember` and its members stay as they were.
//!
//! The roles: `Promote` / `Demote` and a participant's `type` are read by the notice path (chunk B5); here a
//! participant's type is `SuperAdmin` or `Admin` -> `Admin`, anything else -> `Member`. `Pending` and `Requesting` are
//! not read (an admin's view of the group: K8c).
//!
//! WHAT IS NOT STATED HERE: a merge. The metadata names a member by his id in the group's mode and, where the server
//! wrote it, his number; no pair is stated from it (D7: the sources of a merge are the ones its list names, and a group
//! answer is not one of them; the pairs the store holds from `usync` are the merge sweep's, chunk B9).
//!
//! THE ROW (`group_state`, migration 8; [`State`]): what the core was told of the group. [`features`] makes the group's
//! features FROM that row, so every restate of them (a new `features::VERSION`, a `Rebuild`) says the same
//! `group_info` and `cannot_send` the fetch said.
//!
//! Counts only anywhere near this module: a title, a description, a name, a number and an id never reach a log line.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    CannotSend, ConvKey, ConversationFeatures, ConversationPatch, GroupInfo, Member, MemberList,
    MemberRole, Patch, UserId,
};
use wacore_binary::Jid;
use whatsapp_rust::{
    GroupHierarchy, GroupMetadata, GroupMetadataResult, MemberAddMode, ParticipantType,
};

use crate::map::{self, OwnIds};
use crate::notice::TITLE_CAP;

/// A group's description is cut to this many characters (WhatsApp's own limit is 2,048).
pub(crate) const DESCRIPTION_CAP: usize = 4096;

/// The group's rules and our place in it, as the last answered fetch said them. Stored as a bit set
/// (`group_state.rules`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Rules {
    /// Only admins may send (`GroupInfo::announcements_only`).
    pub(crate) announcements_only: bool,
    /// Only admins change the group's name, picture, description and timer (`GroupInfo::attributes_admin_only`).
    pub(crate) locked: bool,
    /// Only admins add members (`GroupInfo::members_admin_only`).
    pub(crate) members_admin_only: bool,
    /// The group was suspended (`GroupInfo::terminated`).
    pub(crate) suspended: bool,
    /// One of our ids is an admin of the group.
    pub(crate) own_admin: bool,
}

impl Rules {
    const ANNOUNCEMENTS_ONLY: i64 = 1;
    const LOCKED: i64 = 2;
    const MEMBERS_ADMIN_ONLY: i64 = 4;
    const SUSPENDED: i64 = 8;
    const OWN_ADMIN: i64 = 16;

    /// The stored form.
    pub(crate) fn bits(self) -> i64 {
        [
            (self.announcements_only, Self::ANNOUNCEMENTS_ONLY),
            (self.locked, Self::LOCKED),
            (self.members_admin_only, Self::MEMBERS_ADMIN_ONLY),
            (self.suspended, Self::SUSPENDED),
            (self.own_admin, Self::OWN_ADMIN),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |bits, (_, bit)| bits | bit)
    }

    /// Reads the stored form; a bit this build does not know is ignored.
    pub(crate) fn from_bits(bits: i64) -> Self {
        Self {
            announcements_only: bits & Self::ANNOUNCEMENTS_ONLY != 0,
            locked: bits & Self::LOCKED != 0,
            members_admin_only: bits & Self::MEMBERS_ADMIN_ONLY != 0,
            suspended: bits & Self::SUSPENDED != 0,
            own_admin: bits & Self::OWN_ADMIN != 0,
        }
    }

    /// The rules after `change` (a notice told it; chunk B5).
    pub(crate) fn with(self, change: Change) -> Self {
        let mut rules = self;
        match change {
            Change::Announcements(on) => rules.announcements_only = on,
            Change::Locked(on) => rules.locked = on,
            Change::MembersAdminOnly(on) => rules.members_admin_only = on,
            Change::Suspended(on) => rules.suspended = on,
            Change::OwnAdmin(on) => rules.own_admin = on,
        }
        rules
    }
}

/// One change of a group's rules or of our place in it that a notice tells (chunk B5): the notice carries the NEW value
/// of one bit of [`Rules`], so the row can follow it without a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Change {
    /// `announcement` / `not_announcement`.
    Announcements(bool),
    /// `locked` / `unlocked`.
    Locked(bool),
    /// `member_add_mode`: `admin_add` is `true`.
    MembersAdminOnly(bool),
    /// `suspended` / `unsuspended`.
    Suspended(bool),
    /// A promote or a demote that names one of our ids.
    OwnAdmin(bool),
}

/// What the core was told of one group (`group_state` without its `dirty_at`, which the fetch policy owns).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct State {
    /// A zone told the core the group's whole member list (a `Found` answer with members, never a notice or a
    /// `Truncated` one): an admin's delete of this group is mapped from then on (D11).
    pub(crate) members_stated: bool,
    /// The title's time in seconds (`s_t`): a title dated before it is never set (M5). `None`: no title was stated with a
    /// time.
    pub(crate) subject_time: Option<u64>,
    /// The disappearing timer in seconds the core was last told by a fetch (0 = told: none). `None`: never.
    pub(crate) timer: Option<u32>,
    /// The contract's version of that statement (`ConversationPatch::timer_version`), in seconds.
    pub(crate) timer_version: Option<u32>,
    /// The last answer for the group was `Truncated`: it gets ONE single query per start.
    pub(crate) truncated: bool,
    /// We are not in the group (`Forbidden`, `NotFound`).
    pub(crate) not_member: bool,
    /// The rules of the last `Found` answer. `None`: never answered.
    pub(crate) rules: Option<Rules>,
    /// The community the last `Found` answer named as this group's parent. `None`: not a subgroup, or never answered.
    pub(crate) parent: Option<ConvKey>,
    /// The core was told [`parent`](Self::parent). `false` with a parent named: it waits, because the core did not know
    /// the parent chat when the answer was stated (the core makes a bare, title-less chat for a parent it does not know),
    /// and the zone that first states the parent group states it for this one ([`withhold_parent`], the zone's release).
    pub(crate) parent_stated: bool,
    /// The time (ms) of the last answered fetch.
    pub(crate) fetched_at: Option<i64>,
}

/// What a fetch needs besides the answer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fetch<'a> {
    /// Our own ids, to find our place among the members through either form.
    pub(crate) own: &'a OwnIds,
    /// When the answer came, in ms: the timer's version (in seconds) and `State::fetched_at`.
    pub(crate) at_ms: i64,
}

/// What an answer means: the events for the core (none for a `Truncated` group) and the row to keep.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outcome {
    /// The group.
    pub(crate) conv: ConvKey,
    /// The events, in order: at most one `ConversationUpsert`.
    pub(crate) events: Vec<BackendEvent>,
    /// The row as it is after the answer.
    pub(crate) state: State,
}

/// The conversation of a group's id; `None` for anything but a group.
pub(crate) fn group_conv(jid: &Jid) -> Option<ConvKey> {
    map::conv_of(jid).filter(|conv| matches!(conv, ConvKey::Group(_)))
}

/// The group an entry of a batch answer is about; `None` for an id that is no group.
pub(crate) fn conv_of(result: &GroupMetadataResult) -> Option<ConvKey> {
    match result {
        GroupMetadataResult::Found(metadata) => group_conv(&metadata.id),
        GroupMetadataResult::Truncated { id, .. }
        | GroupMetadataResult::Forbidden(id)
        | GroupMetadataResult::NotFound(id) => group_conv(id),
        // A variant a later pin adds: nothing is known of it, so nothing is stated.
        _ => None,
    }
}

/// What one entry of a batch answer says, given the row the core was told before (`State::default()` for a group
/// never answered). `None` for an entry about something that is no group.
pub(crate) fn map(
    result: &GroupMetadataResult,
    stored: &State,
    fetch: &Fetch<'_>,
) -> Option<Outcome> {
    let conv = conv_of(result)?;
    Some(match result {
        GroupMetadataResult::Found(metadata) => found(conv, metadata, stored, fetch),
        GroupMetadataResult::Truncated { .. } => truncated(conv, stored, fetch),
        GroupMetadataResult::Forbidden(_) | GroupMetadataResult::NotFound(_) => {
            not_member(conv, stored, fetch)
        }
        // A variant a later pin adds (`conv_of` has no group for it either).
        _ => return None,
    })
}

/// A `Found` answer: the whole state of the group, unless the server marked its list incomplete (fix F2, PROTOCOL-2:
/// `missing_participant_identification`, the same tag upstream documents on a notice as "at least one participant
/// identity was omitted by the server"). Such a list is no snapshot: it states at most an upsert-only delta of the
/// people it names (and only on a list the core holds), never a removal, leaves `members_stated` as it was (so no
/// parked admin delete is released by it), keeps our own place when the list does not name us, and marks the group
/// like a `Truncated` one, so the start's one single query asks it whole.
fn found(conv: ConvKey, metadata: &GroupMetadata, stored: &State, fetch: &Fetch<'_>) -> Outcome {
    let incomplete = metadata.has_missing_participant_identification;
    let mut patch = ConversationPatch::new(conv.clone());
    let mut state = stored.clone();
    state.truncated = incomplete;
    state.not_member = false;
    state.fetched_at = Some(fetch.at_ms);

    // The title: the server's current one, unless the core was told a newer one (M5, T6).
    let title = metadata
        .subject
        .as_deref()
        .map(|text| cut(text, TITLE_CAP))
        .filter(|text| !text.trim().is_empty());
    if let Some(title) = title
        && title_is_current(stored.subject_time, metadata.subject_time)
    {
        patch.title = Patch::Set(title);
        state.subject_time = stored.subject_time.max(metadata.subject_time);
    }

    // The description: the answer is the whole group, so none is none.
    patch.description = metadata
        .description
        .as_deref()
        .map(|text| cut(text, DESCRIPTION_CAP))
        .filter(|text| !text.trim().is_empty())
        .map_or(Patch::Clear, Patch::Set);

    // The members, as a full snapshot; an answer without one is no list (never an empty one). An incomplete list (F2) is
    // a delta that only adds or re-roles, and only on a list the core holds: on none it would show a partial one.
    let (members, place) = members(metadata, fetch.own);
    if !members.is_empty() {
        if !incomplete {
            patch.members = Some(MemberList::snapshot(members));
            state.members_stated = true;
        } else if stored.members_stated {
            patch.members = Some(MemberList {
                full: false,
                upsert: members,
                remove: Vec::new(),
            });
        }
    }
    // Our place: what the list says, but a list that does not name us at all (incomplete) says nothing of it.
    let own_admin = match place {
        Some(admin) => admin,
        None if incomplete => stored.rules.is_some_and(|rules| rules.own_admin),
        None => false,
    };

    // The community parent. The patch says it; whether the core may be told yet is the zone's call
    // ([`withhold_parent`]: the core makes a bare chat for a parent it does not know).
    state.parent = match metadata.hierarchy() {
        GroupHierarchy::Subgroup { parent, .. } => group_conv(&parent),
        _ => None,
    };
    state.parent_stated = state.parent.is_some();
    patch.parent = state.parent.clone().map_or(Patch::Clear, Patch::Set);

    // The timer: no `<ephemeral>` is no timer. The version is the fetch's time (T7): the metadata has none of its own,
    // and without one a late older notice would win in the core.
    let secs = metadata
        .ephemeral
        .and_then(|settings| settings.expiration)
        .unwrap_or(0);
    patch.timer = if secs > 0 {
        Patch::Set(Duration::from_secs(u64::from(secs)))
    } else {
        Patch::Clear
    };
    patch.timer_version = u32::try_from(fetch.at_ms / 1000)
        .ok()
        .filter(|version| *version > 0);
    state.timer = Some(secs);
    state.timer_version = patch.timer_version;

    // The rules, and the features made of them: always together with the members (M4).
    state.rules = Some(Rules {
        announcements_only: metadata.is_announcement,
        locked: metadata.is_locked,
        members_admin_only: metadata.member_add_mode == Some(MemberAddMode::AdminAdd),
        suspended: metadata.is_suspended,
        own_admin,
    });
    patch.features = Some(features(&conv, &state));

    Outcome {
        conv,
        events: vec![BackendEvent::ConversationUpsert(patch)],
        state,
    }
}

/// The community parent the core is not told YET: the zone found no chat of the parent in the core and none stated before
/// this entry in its own answer, so the patch leaves `parent` out and the row names the parent and waits
/// (`parent_stated` false). The zone that first states the parent group restates it for the subgroup. Only an outcome
/// whose answer named a parent is changed.
pub(crate) fn withhold_parent(outcome: &mut Outcome) {
    if outcome.state.parent.is_none() {
        return;
    }
    outcome.state.parent_stated = false;
    for event in &mut outcome.events {
        if let BackendEvent::ConversationUpsert(patch) = event {
            patch.parent = Patch::Unchanged;
        }
    }
}

/// A `Truncated` answer: the server gave the group's size and nothing else. Nothing is stated; the row says the group
/// owes a single query.
fn truncated(conv: ConvKey, stored: &State, fetch: &Fetch<'_>) -> Outcome {
    let mut state = stored.clone();
    state.truncated = true;
    state.fetched_at = Some(fetch.at_ms);
    Outcome {
        conv,
        events: Vec::new(),
        state,
    }
}

/// We are not in the group (`Forbidden`, `NotFound`, or a single query's 403 and 404): the chat is stated `NotMember`.
/// Its members, title and timer stay as the core has them.
pub(crate) fn not_member(conv: ConvKey, stored: &State, fetch: &Fetch<'_>) -> Outcome {
    let mut state = stored.clone();
    state.not_member = true;
    state.truncated = false;
    state.fetched_at = Some(fetch.at_ms);
    let mut patch = ConversationPatch::new(conv.clone());
    patch.features = Some(features(&conv, &state));
    Outcome {
        conv,
        events: vec![BackendEvent::ConversationUpsert(patch)],
        state,
    }
}

/// The group's features: the chat's own (`features::of`, a group is always started) with the rules the last fetch
/// said and the reason we cannot send, if there is one. A group we are not in states `NotMember` and no rules.
/// `AdminsOnly` is stated when only admins may send and none of our ids is one; never `StillLoading` (it would block
/// the group's sends after every start until the batch answers, D10).
pub(crate) fn features(conv: &ConvKey, state: &State) -> ConversationFeatures {
    let mut features = crate::features::of(conv, true);
    if state.not_member {
        features.cannot_send = Some(CannotSend::NotMember);
        // A group the server ended (`delete`, chunk B5) is terminated for everyone; for a group we merely left the
        // rules of an earlier fetch say nothing any more, so none are stated.
        if state.rules.is_some_and(|rules| rules.suspended) {
            features.group_info = Some(GroupInfo {
                terminated: true,
                ..GroupInfo::default()
            });
        }
        return features;
    }
    if let Some(rules) = state.rules {
        features.group_info = Some(GroupInfo {
            attributes_admin_only: rules.locked,
            members_admin_only: rules.members_admin_only,
            announcements_only: rules.announcements_only,
            terminated: rules.suspended,
        });
        if rules.announcements_only && !rules.own_admin {
            features.cannot_send = Some(CannotSend::AdminsOnly);
        }
        // A suspended group is ended for everybody (`terminated`: "nobody can send"), and the core gates sends on
        // `cannot_send` alone, so the composer, the timer menu and an admin's delete must be refused here. The contract has
        // no kind of its own; the nearest is `NotMember`, as for a group the server deleted (fix F4, PROTOCOL-7).
        if rules.suspended {
            features.cannot_send = Some(CannotSend::NotMember);
        }
    }
    features
}

/// Whether a title dated `incoming` may be set when the core was told one dated `stored`: not when it is older (M5). A
/// side with no time compares as nothing (T6): never as zero, which would call it older.
pub(crate) fn title_is_current(stored: Option<u64>, incoming: Option<u64>) -> bool {
    match (stored, incoming) {
        (Some(stored), Some(incoming)) => incoming >= stored,
        _ => true,
    }
}

/// The members of the answer, each person once in the answer's order (a person named twice keeps the higher role), and
/// our place in the list: `Some(true)` when one of our ids is an admin, `Some(false)` when we are listed and none is,
/// `None` when the list does not name us. A participant is us when his id, his number or his LID is one of ours (the
/// list may name us by either form).
fn members(metadata: &GroupMetadata, own: &OwnIds) -> (Vec<Member>, Option<bool>) {
    let mut members: Vec<Member> = Vec::with_capacity(metadata.participants.len());
    let mut place: Option<bool> = None;
    for participant in &metadata.participants {
        let role = role_of(&participant.participant_type);
        let ours = [
            Some(&participant.jid),
            participant.phone_number.as_ref(),
            participant.lid.as_ref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(map::person_of)
        .any(|person| own.is_own(&person));
        if ours {
            place = Some(place.unwrap_or(false) || role == MemberRole::Admin);
        }
        let Some(user) = map::person_of(&participant.jid) else {
            continue;
        };
        add(&mut members, user, role);
    }
    (members, place)
}

/// Adds `user` unless he is listed already; a second entry may only raise his role.
fn add(members: &mut Vec<Member>, user: UserId, role: MemberRole) {
    match members.iter_mut().find(|member| member.user == user) {
        Some(known) => known.role = known.role.max(role),
        None => members.push(Member { user, role }),
    }
}

/// The core's role of a participant's type: the contract has no owner, so a super admin is an `Admin` (D10).
fn role_of(kind: &ParticipantType) -> MemberRole {
    match kind {
        ParticipantType::SuperAdmin | ParticipantType::Admin => MemberRole::Admin,
        _ => MemberRole::Member,
    }
}

/// `text` cut to `cap` characters.
fn cut(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

pub(crate) mod fetch;
pub(crate) mod zone;

#[cfg(test)]
mod tests;
