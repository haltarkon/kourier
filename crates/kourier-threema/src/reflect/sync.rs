//! THE SYNC DELTAS (TH5 A7a; brief §2.6 as amended by `th5-decisions.md` §1.7, H6, M1, L3): a reflected
//! `ContactSync` / `GroupSync` applied FIELD BY FIELD to the stored `d2d_sync.Contact` / `d2d_sync.Group`
//! (schema 2's BLOBs), never as a prost merge (prost APPENDS the repeated field of a present sub-message):
//!
//! - a PRESENT field or sub-message REPLACES the stored one WHOLE (`member_identities`, the pictures, the
//!   overrides, `work_availability_status`); an ABSENT one leaves it unchanged; a repeated field is
//!   replaced, never appended ("only provide fields ... whose values have changed", `md-d2d-sync.proto`);
//! - EVERY field of the delta is named by an exhaustive destructuring (no `..`): a field a libthreema update
//!   adds to the pinned proto does not compile here until it is handled (L3);
//! - H6: a stored contact's `public_key` is KEPT ("must be ignored (a public key for an identity cannot be
//!   changed)"); a delta with a DIFFERENT key is applied without it and counted as the WARN
//!   [`WARN_KEY_CHANGE`] (rule name only);
//! - M1: a present EMPTY name is a value (it clears; `join::events` states `Patch::Clear`).
//!
//! THE ACTIONS. `ContactSync.Create` of an identity the file holds is an UPDATE (Desktop discards it, "contact
//! already exists"; Kourier applies what it says, the phone's state wins); `ContactSync.Update` of an unknown
//! identity WITH a 32-byte key is a create (brief §2.6; Desktop discards it), without one it is DROPPED
//! [`RULE_CONTACT_UNKNOWN`]. The proto has no `ContactSync.Delete`: a removed contact is an update to the
//! acquaintance level `GROUP_OR_DELETED` (stated `hidden`). `GroupSync.Create` of a group the file holds is
//! an UPDATE of its fields with NO notice (Desktop's `groups.add` would refuse the duplicate);
//! `GroupSync.Update` of an unknown group is DROPPED `group-unknown` (as Desktop); `GroupSync.Delete` deletes
//! the row and states `ConversationRemoved` (L10: the chat's history goes too, as on the phone), an unknown
//! one is DROPPED `group-unknown`. That is the ONLY removal: an Update that makes a group not visible (M1;
//! KICKED or LEFT with nobody listed) writes the row and states nothing (round A, M-1), as a hidden contact.
//!
//! VALIDATION (the spec's "discard and log, but still acknowledge"): an identity or a member that is no
//! Threema ID, an unknown enum value of a top-level field (the overrides are stored whole, their inner
//! enums are read by the step that states them), a new contact's key that is not 32 bytes, the own ID as a
//! contact, a sync without its message, and TH4's bounds (100,000 contacts, 10,000 groups, 10,000 members
//! of a group) -> the envelope is DROPPED by the rule's name, nothing written (never a partial write).
//!
//! THE SETTINGS, THE PROFILE AND THE LISTS (TH5b B4, decision R13), in the same form: [`apply_settings`],
//! [`apply_profile`] and [`apply_list`] are pure and destructure the delta EXHAUSTIVELY; a present field or
//! sub-message replaces the stored one WHOLE (`blocked_identities`, `exclude_from_sync_identities` and a
//! list's `member_identities` are replaced, never appended), an absent one keeps it; a file without the
//! settings or profile row (a TH1 file) starts from the empty message. The rows are written by the zone
//! ([`Write::Settings`], [`Write::Profile`], [`Write::List`], [`Write::ListDeleted`]): nothing else writes
//! `settings`, `own_profile` or `dist_list` on the receive path. WHAT THEY STATE:
//!
//! - a `SettingsSync` -> `AccountSettings` of the WHOLE new message (`join::events::settings_patch`, as the
//!   join and a Rebuild) and, for every STORED contact whose blocked state changed (in the old blocked list
//!   or the new one, not both; by ID), its `ContactUpsert` from the whole stored contact with the new
//!   blocked flag (`join::events::contact_event`); a blocked ID the file holds no contact for states nothing;
//! - a `UserProfileSync` -> `AccountReady { own_ids: [own], device: None, display_name, number: None }` of
//!   the whole new message (`store::account_ready`, as the join and a Rebuild): the nickname, or `None` when it
//!   is empty. An empty nickname CLEARS the stored one (M1's rule for names: the spec stores it as-is). CONTRACT
//!   LIMIT (F5): the core `coalesce`s `display_name`, so a cleared nickname leaves the core's old display name
//!   in place until a later one is set (the contract has no way to clear it; recorded, R13). The picture is
//!   stored, not stated (TH8). The sub-messages (picture, share policy, links, availability) are stored whole
//!   and read by the step that uses them, as A7a's overrides;
//! - a `DistributionListSync` -> NOTHING stated (a later lists step states lists; the join states none
//!   either). `Create` of a held list is an UPDATE; `Update` or `Delete` of an unknown one is DROPPED
//!   [`RULE_LIST_UNKNOWN`]; an EMPTY member list (the spec: "not valid"), a new list without members, or a
//!   member that is no Threema ID is DROPPED [`RULE_LIST_INVALID`].
//!
//! Validation as A7a's: the spec's strict enums of the settings ([`RULE_SETTINGS_ENUM`]) and of a list
//! ([`RULE_LIST_ENUM`]), a blocked or excluded entry that is no Threema ID ([`RULE_SETTINGS_INVALID`], as
//! Desktop's `Identities` schema), TH4's bounds (100,000 blocked and 100,000 excluded IDs, 10,000 lists,
//! 10,000 members of a list) -> `over-bound` (N passes, N+1 is dropped); a sync without its action or message
//! -> `sync-empty`. Nothing is written for a dropped delta.
//!
//! Pure: [`map`](super::map) reads the file through its `View` and the zone writes what this returns.

use std::collections::{HashMap, HashSet};

use kourier_backend_api::BackendEvent;
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::group_sync::update::MemberStateChange;
use libthreema::protobuf::d2d_sync::{self, contact, group, settings};

use super::body;
use super::map::{Cx, Mapped, View, Write};
use super::notice;
use crate::error::Result;
use crate::join::events::{self, SnapGroup};

/// Rule: a contact identity that is no Threema ID.
pub(crate) const RULE_CONTACT_INVALID: &str = "contact-invalid";
/// Rule: the own ID as a contact.
pub(crate) const RULE_CONTACT_OWN: &str = "contact-own";
/// Rule: an update of a contact the file does not hold, without a key to create it.
pub(crate) const RULE_CONTACT_UNKNOWN: &str = "contact-unknown";
/// Rule: a new contact whose public key is not 32 bytes.
pub(crate) const RULE_CONTACT_KEY: &str = "contact-key";
/// Rule: an enum value of a contact the pinned proto does not know.
pub(crate) const RULE_CONTACT_ENUM: &str = "contact-enum";
/// H6's counted WARN: a delta's different key for an existing contact, ignored.
pub(crate) const WARN_KEY_CHANGE: &str = "contact-key-change";
/// Rule: a group without its identity, or a creator that is no Threema ID.
pub(crate) const RULE_GROUP_INVALID: &str = "group-invalid";
/// Rule: a member (or a member state change) that is no Threema ID.
pub(crate) const RULE_GROUP_MEMBER: &str = "group-member-invalid";
/// Rule: an enum value of a group (or a member state change) the pinned proto does not know.
pub(crate) const RULE_GROUP_ENUM: &str = "group-enum";
/// Rule: an update or delete of a group the file does not hold (as the map's message rule).
pub(crate) const RULE_GROUP_UNKNOWN: &str = "group-unknown";
/// Rule: a sync whose action or message is missing.
pub(crate) const RULE_SYNC_EMPTY: &str = "sync-empty";
/// Rule: a sync that would pass one of TH4's bounds.
pub(crate) const RULE_OVER_BOUND: &str = "over-bound";

/// Rule (B4): a settings enum value the pinned proto does not know (the spec's strict enums).
pub(crate) const RULE_SETTINGS_ENUM: &str = "settings-enum";
/// Rule (B4): a blocked or excluded identity that is no Threema ID.
pub(crate) const RULE_SETTINGS_INVALID: &str = "settings-invalid";
/// Rule (B4, R13): an update or delete of a distribution list the file does not hold.
pub(crate) const RULE_LIST_UNKNOWN: &str = "list-unknown";
/// Rule (B4, R13): an empty member list, a new list without members, or a member that is no Threema ID.
pub(crate) const RULE_LIST_INVALID: &str = "list-invalid";
/// Rule (B4): a list enum value the pinned proto does not know.
pub(crate) const RULE_LIST_ENUM: &str = "list-enum";

/// TH4's bounds (`join::essential`): the most contacts, groups, members of one group (and of one list),
/// distribution lists, and blocked (and excluded) IDs of the settings.
const MAX_CONTACTS: u64 = 100_000;
const MAX_GROUPS: u64 = 10_000;
const MAX_MEMBERS: usize = 10_000;
const MAX_LISTS: u64 = 10_000;
const MAX_IDENTITIES: usize = 100_000;

/// A Threema public key (32 bytes).
const KEY_LEN: usize = 32;

/// A present value replaces the stored one (a scalar or a sub-message WHOLE); an absent one keeps it.
fn put<T>(stored: &mut Option<T>, delta: Option<T>) {
    if delta.is_some() {
        *stored = delta;
    }
}

/// An absent enum value, or one the generated enum knows.
fn known<E: TryFrom<i32>>(value: Option<i32>) -> bool {
    value.is_none_or(|v| E::try_from(v).is_ok())
}

/// What a contact delta did.
#[derive(Clone, PartialEq)]
pub(crate) struct ContactApplied {
    /// The new stored message, whole.
    pub(crate) contact: d2d_sync::Contact,
    /// The file did not hold the contact.
    pub(crate) created: bool,
    /// H6: the delta carried a different key for an existing contact; it was ignored.
    pub(crate) key_ignored: bool,
}

/// THE CONTACT DELTA (module doc): `stored` is the file's message of the identity (`None`: not held).
#[expect(
    deprecated,
    reason = "the delta names EVERY field (L3), the deprecated sound override too: it is kept whole, never read"
)]
pub(crate) fn apply_contact(
    stored: Option<&d2d_sync::Contact>,
    delta: d2d_sync::Contact,
) -> std::result::Result<ContactApplied, &'static str> {
    let d2d_sync::Contact {
        identity,
        public_key,
        created_at,
        first_name,
        last_name,
        nickname,
        verification_level,
        work_verification_level,
        identity_type,
        acquaintance_level,
        activity_state,
        feature_mask,
        sync_state,
        contact_defined_profile_picture,
        user_defined_profile_picture,
        work_last_full_sync_at,
        work_availability_status,
        read_receipt_policy_override,
        typing_indicator_policy_override,
        notification_trigger_policy_override,
        deprecated_notification_sound_policy_override,
        conversation_category,
        conversation_visibility,
    } = delta;
    if !body::is_identity(&identity) {
        return Err(RULE_CONTACT_INVALID);
    }
    let enums = known::<contact::VerificationLevel>(verification_level)
        && known::<contact::WorkVerificationLevel>(work_verification_level)
        && known::<contact::IdentityType>(identity_type)
        && known::<contact::AcquaintanceLevel>(acquaintance_level)
        && known::<contact::ActivityState>(activity_state)
        && known::<contact::SyncState>(sync_state)
        && known::<d2d_sync::ConversationCategory>(conversation_category)
        && known::<d2d_sync::ConversationVisibility>(conversation_visibility);
    if !enums {
        return Err(RULE_CONTACT_ENUM);
    }
    let (mut new, created, key_ignored) = match stored {
        Some(stored) => {
            if stored.identity != identity {
                return Err(RULE_CONTACT_INVALID);
            }
            // H6: the stored key stays; a different one is counted, the rest applied.
            let differs = public_key
                .as_ref()
                .is_some_and(|k| Some(k) != stored.public_key.as_ref());
            (stored.clone(), false, differs)
        }
        None => {
            let Some(key) = public_key else {
                return Err(RULE_CONTACT_UNKNOWN);
            };
            if key.len() != KEY_LEN {
                return Err(RULE_CONTACT_KEY);
            }
            let new = d2d_sync::Contact {
                identity,
                public_key: Some(key),
                ..d2d_sync::Contact::default()
            };
            (new, true, false)
        }
    };
    put(&mut new.created_at, created_at);
    put(&mut new.first_name, first_name);
    put(&mut new.last_name, last_name);
    put(&mut new.nickname, nickname);
    put(&mut new.verification_level, verification_level);
    put(&mut new.work_verification_level, work_verification_level);
    put(&mut new.identity_type, identity_type);
    put(&mut new.acquaintance_level, acquaintance_level);
    put(&mut new.activity_state, activity_state);
    put(&mut new.feature_mask, feature_mask);
    put(&mut new.sync_state, sync_state);
    put(
        &mut new.contact_defined_profile_picture,
        contact_defined_profile_picture,
    );
    put(
        &mut new.user_defined_profile_picture,
        user_defined_profile_picture,
    );
    put(&mut new.work_last_full_sync_at, work_last_full_sync_at);
    put(&mut new.work_availability_status, work_availability_status);
    put(
        &mut new.read_receipt_policy_override,
        read_receipt_policy_override,
    );
    put(
        &mut new.typing_indicator_policy_override,
        typing_indicator_policy_override,
    );
    put(
        &mut new.notification_trigger_policy_override,
        notification_trigger_policy_override,
    );
    put(
        &mut new.deprecated_notification_sound_policy_override,
        deprecated_notification_sound_policy_override,
    );
    put(&mut new.conversation_category, conversation_category);
    put(&mut new.conversation_visibility, conversation_visibility);
    Ok(ContactApplied {
        contact: new,
        created,
        key_ignored,
    })
}

/// What a group delta did (what the notices are made of).
#[derive(Clone, PartialEq)]
pub(crate) struct GroupApplied {
    /// The new stored message, whole.
    pub(crate) group: d2d_sync::Group,
    /// The file did not hold the group.
    pub(crate) created: bool,
    /// The delta's name when it differs from the stored one (an empty one too: M1), else `None`.
    pub(crate) title: Option<String>,
    /// The delta carried a profile picture (set or removed).
    pub(crate) picture: bool,
}

/// The creator and group id of a group identity, or the rule that drops it.
pub(crate) fn identity_of(
    id: Option<&libthreema::protobuf::common::GroupIdentity>,
) -> std::result::Result<(String, u64), &'static str> {
    match id {
        Some(id) if body::is_identity(&id.creator_identity) => {
            Ok((id.creator_identity.clone(), id.group_id))
        }
        _ => Err(RULE_GROUP_INVALID),
    }
}

/// THE GROUP DELTA (module doc): `stored` is the file's message of the group (`None`: not held).
#[expect(
    deprecated,
    reason = "the delta names EVERY field (L3), the deprecated sound override too: it is kept whole, never read"
)]
pub(crate) fn apply_group(
    stored: Option<&d2d_sync::Group>,
    delta: d2d_sync::Group,
) -> std::result::Result<GroupApplied, &'static str> {
    let d2d_sync::Group {
        group_identity,
        name,
        created_at,
        user_state,
        profile_picture,
        member_identities,
        notification_trigger_policy_override,
        deprecated_notification_sound_policy_override,
        conversation_category,
        conversation_visibility,
    } = delta;
    let (creator, group_id) = identity_of(group_identity.as_ref())?;
    if let Some(members) = &member_identities {
        if members.identities.len() > MAX_MEMBERS {
            return Err(RULE_OVER_BOUND);
        }
        if !members.identities.iter().all(|m| body::is_identity(m)) {
            return Err(RULE_GROUP_MEMBER);
        }
    }
    let enums = known::<group::UserState>(user_state)
        && known::<d2d_sync::ConversationCategory>(conversation_category)
        && known::<d2d_sync::ConversationVisibility>(conversation_visibility);
    if !enums {
        return Err(RULE_GROUP_ENUM);
    }
    let (mut new, created) = match stored {
        Some(stored) => {
            if identity_of(stored.group_identity.as_ref()) != Ok((creator, group_id)) {
                return Err(RULE_GROUP_INVALID);
            }
            (stored.clone(), false)
        }
        None => (
            d2d_sync::Group {
                group_identity,
                ..d2d_sync::Group::default()
            },
            true,
        ),
    };
    let title = name
        .as_ref()
        .filter(|n| new.name.as_deref().unwrap_or("") != n.as_str())
        .cloned();
    let picture = profile_picture.is_some();
    put(&mut new.name, name);
    put(&mut new.created_at, created_at);
    put(&mut new.user_state, user_state);
    put(&mut new.profile_picture, profile_picture);
    put(&mut new.member_identities, member_identities);
    put(
        &mut new.notification_trigger_policy_override,
        notification_trigger_policy_override,
    );
    put(
        &mut new.deprecated_notification_sound_policy_override,
        deprecated_notification_sound_policy_override,
    );
    put(&mut new.conversation_category, conversation_category);
    put(&mut new.conversation_visibility, conversation_visibility);
    Ok(GroupApplied {
        group: new,
        created,
        title,
        picture,
    })
}

/// A `ContactSync` (module doc): the BLOB written back, the contact's `ContactUpsert` and, while the contact
/// is DIRECT, its 1:1 chat's `ConversationUpsert` after it, both from the WHOLE new message
/// (`join::events::contact_event`, `direct_event`, so a Rebuild states the same).
pub(crate) fn contact_sync(
    sync: &d2d::ContactSync,
    cx: &Cx<'_>,
    view: &dyn View,
) -> Result<Mapped> {
    use d2d::contact_sync::Action;
    let delta = match &sync.action {
        Some(Action::Create(d2d::contact_sync::Create { contact })) => contact.clone(),
        Some(Action::Update(d2d::contact_sync::Update { contact })) => contact.clone(),
        None => None,
    };
    let Some(delta) = delta else {
        return Ok(Mapped::dropped(RULE_SYNC_EMPTY));
    };
    if delta.identity == cx.own {
        return Ok(Mapped::dropped(RULE_CONTACT_OWN));
    }
    let stored = if body::is_identity(&delta.identity) {
        view.stored_contact(&delta.identity)?
    } else {
        None
    };
    if stored.is_none() && view.sync_counts()?.0 >= MAX_CONTACTS {
        return Ok(Mapped::dropped(RULE_OVER_BOUND));
    }
    let applied = match apply_contact(stored.as_ref(), delta) {
        Ok(applied) => applied,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let blocked = view.blocked(&applied.contact.identity)?;
    let blocked_set: std::collections::HashSet<&str> = if blocked {
        std::iter::once(applied.contact.identity.as_str()).collect()
    } else {
        std::collections::HashSet::new()
    };
    let Ok(event) = events::contact_event(&applied.contact, &blocked_set) else {
        return Ok(Mapped::dropped(RULE_CONTACT_INVALID));
    };
    // E1 (A7b): the 1:1 chat of a contact that is DIRECT after the delta (created, un-hidden, or its chat's
    // state changed), from the WHOLE new message as the join and a Rebuild state it; a contact the delta
    // hides states nothing for its chat (the lead's call, `events::direct_patch`).
    let Ok(direct) = events::direct_event(&applied.contact) else {
        return Ok(Mapped::dropped(RULE_CONTACT_INVALID));
    };
    let mut mapped = Mapped::events(std::iter::once(event).chain(direct).collect());
    if applied.key_ignored {
        mapped.warned = Some(WARN_KEY_CHANGE);
    }
    mapped
        .writes
        .push(Write::Contact(Box::new(applied.contact)));
    Ok(mapped)
}

/// A `GroupSync` (module doc): the BLOB written back (or deleted), the group's `ConversationUpsert` from the
/// WHOLE new message (`join::events::group_event`, so a Rebuild states the same), and an Update's NOTICES
/// ([`notice`]) after it in the same row. A group that is not visible (M1) states NOTHING (round A, M-1: the
/// row is written, the chat stays as the core has it, never `ConversationRemoved`, which would delete its
/// history; the contract has no hide, as for a hidden contact); `GroupSync.Delete` alone removes the chat.
pub(crate) fn group_sync(sync: &d2d::GroupSync, cx: &Cx<'_>, view: &dyn View) -> Result<Mapped> {
    use d2d::group_sync::Action;
    let empty = HashMap::new();
    let (delta, changes, is_update) = match &sync.action {
        Some(Action::Create(d2d::group_sync::Create { group })) => (group.clone(), &empty, false),
        Some(Action::Update(d2d::group_sync::Update {
            group,
            member_state_changes,
        })) => (group.clone(), member_state_changes, true),
        Some(Action::Delete(d2d::group_sync::Delete { group_identity })) => {
            return group_delete(group_identity.as_ref(), view);
        }
        None => (None, &empty, false),
    };
    let Some(delta) = delta else {
        return Ok(Mapped::dropped(RULE_SYNC_EMPTY));
    };
    let (creator, group_id) = match identity_of(delta.group_identity.as_ref()) {
        Ok(id) => id,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    // The member state changes are validated before anything is written (strict enums, IDs).
    for (identity, change) in changes {
        if !body::is_identity(identity) {
            return Ok(Mapped::dropped(RULE_GROUP_MEMBER));
        }
        if MemberStateChange::try_from(*change).is_err() {
            return Ok(Mapped::dropped(RULE_GROUP_ENUM));
        }
    }
    let stored = view.stored_group(&creator, group_id)?;
    if stored.is_none() {
        if is_update {
            return Ok(Mapped::dropped(RULE_GROUP_UNKNOWN));
        }
        if view.sync_counts()?.1 >= MAX_GROUPS {
            return Ok(Mapped::dropped(RULE_OVER_BOUND));
        }
    }
    let applied = match apply_group(stored.as_ref(), delta) {
        Ok(applied) => applied,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let snap = SnapGroup {
        creator: creator.clone(),
        group_id,
        sync: applied.group.clone(),
    };
    let Ok(stated) = events::group_event(&snap, cx.own) else {
        return Ok(Mapped::dropped(RULE_GROUP_INVALID));
    };
    let Ok(conv) = events::group_key(&creator, group_id) else {
        return Ok(Mapped::dropped(RULE_GROUP_INVALID));
    };
    let mut out = Vec::new();
    // M1 (round A, M-1): a group that is not visible (`stated` is `None`) states NOTHING, whether it was
    // visible before or not: never `ConversationRemoved`.
    if let Some(event) = stated {
        out.push(event);
        // Notices: an UPDATE of a stored group only (a Create shows none, as Desktop).
        if let (true, Some(old)) = (is_update, stored.as_ref()) {
            let kinds = notice::group_notices(&notice::Change {
                old,
                new: &applied.group,
                creator: &creator,
                own: cx.own,
                changes,
                title: applied.title.as_deref(),
                picture: applied.picture,
            });
            out.extend(notice::rows(kinds, &conv, &creator, cx));
        }
    }
    let mut mapped = Mapped::events(out);
    mapped.writes.push(Write::Group {
        creator,
        group_id,
        sync: Box::new(applied.group),
    });
    Ok(mapped)
}

/// `GroupSync.Delete`: the row deleted, `ConversationRemoved` (L10).
fn group_delete(
    id: Option<&libthreema::protobuf::common::GroupIdentity>,
    view: &dyn View,
) -> Result<Mapped> {
    let (creator, group_id) = match identity_of(id) {
        Ok(id) => id,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    if view.stored_group(&creator, group_id)?.is_none() {
        return Ok(Mapped::dropped(RULE_GROUP_UNKNOWN));
    }
    let Ok(conv) = events::group_key(&creator, group_id) else {
        return Ok(Mapped::dropped(RULE_GROUP_INVALID));
    };
    let mut mapped = Mapped::events(vec![BackendEvent::ConversationRemoved { conv }]);
    mapped
        .writes
        .push(Write::GroupDeleted { creator, group_id });
    Ok(mapped)
}

/// A list of Threema IDs of the settings (`blocked_identities`, `exclude_from_sync_identities`): within TH4's
/// bound and every entry an ID, or the rule that drops the delta.
fn identities_ok(
    list: Option<&libthreema::protobuf::common::Identities>,
) -> std::result::Result<(), &'static str> {
    let Some(list) = list else {
        return Ok(());
    };
    if list.identities.len() > MAX_IDENTITIES {
        return Err(RULE_OVER_BOUND);
    }
    if !list.identities.iter().all(|i| body::is_identity(i)) {
        return Err(RULE_SETTINGS_INVALID);
    }
    Ok(())
}

/// THE SETTINGS DELTA (module doc, B4): `stored` is the file's settings (`None`: no row yet).
#[expect(
    deprecated,
    reason = "the delta names EVERY field (L3), the deprecated contact sync and keyboard policies too: kept whole, never read"
)]
pub(crate) fn apply_settings(
    stored: Option<&d2d_sync::Settings>,
    delta: d2d_sync::Settings,
) -> std::result::Result<d2d_sync::Settings, &'static str> {
    let d2d_sync::Settings {
        deprecated_contact_sync_policy,
        unknown_contact_policy,
        read_receipt_policy,
        typing_indicator_policy,
        o2o_call_policy,
        o2o_call_connection_policy,
        o2o_call_video_policy,
        group_call_policy,
        screenshot_policy,
        deprecated_keyboard_data_collection_policy,
        blocked_identities,
        exclude_from_sync_identities,
    } = delta;
    let enums = known::<settings::DeprecatedContactSyncPolicy>(deprecated_contact_sync_policy)
        && known::<settings::UnknownContactPolicy>(unknown_contact_policy)
        && known::<d2d_sync::ReadReceiptPolicy>(read_receipt_policy)
        && known::<d2d_sync::TypingIndicatorPolicy>(typing_indicator_policy)
        && known::<settings::O2oCallPolicy>(o2o_call_policy)
        && known::<settings::O2oCallConnectionPolicy>(o2o_call_connection_policy)
        && known::<settings::O2oCallVideoPolicy>(o2o_call_video_policy)
        && known::<settings::GroupCallPolicy>(group_call_policy)
        && known::<settings::ScreenshotPolicy>(screenshot_policy)
        && known::<settings::DeprecatedKeyboardDataCollectionPolicy>(
            deprecated_keyboard_data_collection_policy,
        );
    if !enums {
        return Err(RULE_SETTINGS_ENUM);
    }
    identities_ok(blocked_identities.as_ref())?;
    identities_ok(exclude_from_sync_identities.as_ref())?;
    let mut new = stored.cloned().unwrap_or_default();
    put(
        &mut new.deprecated_contact_sync_policy,
        deprecated_contact_sync_policy,
    );
    put(&mut new.unknown_contact_policy, unknown_contact_policy);
    put(&mut new.read_receipt_policy, read_receipt_policy);
    put(&mut new.typing_indicator_policy, typing_indicator_policy);
    put(&mut new.o2o_call_policy, o2o_call_policy);
    put(
        &mut new.o2o_call_connection_policy,
        o2o_call_connection_policy,
    );
    put(&mut new.o2o_call_video_policy, o2o_call_video_policy);
    put(&mut new.group_call_policy, group_call_policy);
    put(&mut new.screenshot_policy, screenshot_policy);
    put(
        &mut new.deprecated_keyboard_data_collection_policy,
        deprecated_keyboard_data_collection_policy,
    );
    put(&mut new.blocked_identities, blocked_identities);
    put(
        &mut new.exclude_from_sync_identities,
        exclude_from_sync_identities,
    );
    Ok(new)
}

/// The blocked IDs of `settings` (none without a row or a list).
fn blocked_of(settings: Option<&d2d_sync::Settings>) -> HashSet<&str> {
    settings
        .and_then(|s| s.blocked_identities.as_ref())
        .map(|b| b.identities.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

/// A `SettingsSync` (module doc): the row written back, `AccountSettings` of the whole new message, then the
/// `ContactUpsert` of every stored contact whose blocked state changed, by ID.
pub(crate) fn settings_sync(sync: &d2d::SettingsSync, view: &dyn View) -> Result<Mapped> {
    use d2d::settings_sync::Action;
    let delta = match &sync.action {
        Some(Action::Update(d2d::settings_sync::Update { settings })) => settings.clone(),
        None => None,
    };
    let Some(delta) = delta else {
        return Ok(Mapped::dropped(RULE_SYNC_EMPTY));
    };
    let stored = view.stored_settings()?;
    let new = match apply_settings(stored.as_ref(), delta) {
        Ok(new) => new,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let mut out = vec![BackendEvent::AccountSettings(events::settings_patch(&new))];
    let old_blocked = blocked_of(stored.as_ref());
    let new_blocked = blocked_of(Some(&new));
    // In one list and not the other: the contacts whose blocked state changed.
    let mut changed: Vec<&str> = old_blocked
        .symmetric_difference(&new_blocked)
        .copied()
        .collect();
    changed.sort_unstable();
    for identity in changed {
        let Some(contact) = view.stored_contact(identity)? else {
            continue;
        };
        let Ok(event) = events::contact_event(&contact, &new_blocked) else {
            return Ok(Mapped::dropped(RULE_CONTACT_INVALID));
        };
        out.push(event);
    }
    let mut mapped = Mapped::events(out);
    mapped.writes.push(Write::Settings(Box::new(new)));
    Ok(mapped)
}

/// THE PROFILE DELTA (module doc, B4): `stored` is the file's profile (`None`: no row yet). Nothing to refuse:
/// the nickname is any text (an empty one clears), the sub-messages are stored whole.
pub(crate) fn apply_profile(
    stored: Option<&d2d_sync::UserProfile>,
    delta: d2d_sync::UserProfile,
) -> d2d_sync::UserProfile {
    let d2d_sync::UserProfile {
        nickname,
        profile_picture,
        profile_picture_share_with,
        identity_links,
        work_availability_status,
    } = delta;
    let mut new = stored.cloned().unwrap_or_default();
    put(&mut new.nickname, nickname);
    put(&mut new.profile_picture, profile_picture);
    put(
        &mut new.profile_picture_share_with,
        profile_picture_share_with,
    );
    put(&mut new.identity_links, identity_links);
    put(&mut new.work_availability_status, work_availability_status);
    new
}

/// A `UserProfileSync` (module doc): the row written back and `AccountReady` with the whole new message's
/// nickname (`None` when it is empty: F5's contract limit, the core keeps its old display name).
pub(crate) fn profile_sync(
    sync: &d2d::UserProfileSync,
    cx: &Cx<'_>,
    view: &dyn View,
) -> Result<Mapped> {
    use d2d::user_profile_sync::Action;
    let delta = match &sync.action {
        Some(Action::Update(d2d::user_profile_sync::Update { user_profile })) => {
            user_profile.clone()
        }
        None => None,
    };
    let Some(delta) = delta else {
        return Ok(Mapped::dropped(RULE_SYNC_EMPTY));
    };
    let new = apply_profile(view.stored_profile()?.as_ref(), delta);
    let ready = crate::store::account_ready(cx.own, new.nickname.as_deref())?;
    let mut mapped = Mapped::events(vec![ready]);
    mapped.writes.push(Write::Profile(Box::new(new)));
    Ok(mapped)
}

/// THE LIST DELTA (module doc, B4): `stored` is the file's list of the delta's id (`None`: not held).
pub(crate) fn apply_list(
    stored: Option<&d2d_sync::DistributionList>,
    delta: d2d_sync::DistributionList,
) -> std::result::Result<d2d_sync::DistributionList, &'static str> {
    let d2d_sync::DistributionList {
        distribution_list_id,
        name,
        created_at,
        member_identities,
        conversation_category,
        conversation_visibility,
    } = delta;
    if let Some(members) = &member_identities {
        // "An empty list is **not** valid" (`md-d2d-sync.proto`).
        if members.identities.is_empty() {
            return Err(RULE_LIST_INVALID);
        }
        if members.identities.len() > MAX_MEMBERS {
            return Err(RULE_OVER_BOUND);
        }
        if !members.identities.iter().all(|m| body::is_identity(m)) {
            return Err(RULE_LIST_INVALID);
        }
    }
    let enums = known::<d2d_sync::ConversationCategory>(conversation_category)
        && known::<d2d_sync::ConversationVisibility>(conversation_visibility);
    if !enums {
        return Err(RULE_LIST_ENUM);
    }
    let mut new = match stored {
        Some(stored) => {
            if stored.distribution_list_id != distribution_list_id {
                return Err(RULE_LIST_INVALID);
            }
            stored.clone()
        }
        // A new list without members would be the empty list the spec forbids.
        None if member_identities.is_none() => return Err(RULE_LIST_INVALID),
        None => d2d_sync::DistributionList {
            distribution_list_id,
            ..d2d_sync::DistributionList::default()
        },
    };
    put(&mut new.name, name);
    put(&mut new.created_at, created_at);
    put(&mut new.member_identities, member_identities);
    put(&mut new.conversation_category, conversation_category);
    put(&mut new.conversation_visibility, conversation_visibility);
    Ok(new)
}

/// A `DistributionListSync` (module doc): the row written back or deleted, NOTHING stated.
pub(crate) fn list_sync(sync: &d2d::DistributionListSync, view: &dyn View) -> Result<Mapped> {
    use d2d::distribution_list_sync::Action;
    let (delta, is_update) = match &sync.action {
        Some(Action::Create(d2d::distribution_list_sync::Create { distribution_list })) => {
            (distribution_list.clone(), false)
        }
        Some(Action::Update(d2d::distribution_list_sync::Update { distribution_list })) => {
            (distribution_list.clone(), true)
        }
        Some(Action::Delete(d2d::distribution_list_sync::Delete {
            distribution_list_id,
        })) => {
            let id = *distribution_list_id;
            if view.stored_list(id)?.is_none() {
                return Ok(Mapped::dropped(RULE_LIST_UNKNOWN));
            }
            let mut mapped = Mapped::events(Vec::new());
            mapped.writes.push(Write::ListDeleted { id });
            return Ok(mapped);
        }
        None => (None, false),
    };
    let Some(delta) = delta else {
        return Ok(Mapped::dropped(RULE_SYNC_EMPTY));
    };
    let stored = view.stored_list(delta.distribution_list_id)?;
    if stored.is_none() {
        if is_update {
            return Ok(Mapped::dropped(RULE_LIST_UNKNOWN));
        }
        if view.list_count()? >= MAX_LISTS {
            return Ok(Mapped::dropped(RULE_OVER_BOUND));
        }
    }
    let new = match apply_list(stored.as_ref(), delta) {
        Ok(new) => new,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let mut mapped = Mapped::events(Vec::new());
    mapped.writes.push(Write::List(Box::new(new)));
    Ok(mapped)
}

#[cfg(test)]
#[expect(
    deprecated,
    reason = "the field table names every field, the deprecated sound override too"
)]
mod tests {
    use kourier_model::{ConversationPatch, Patch};
    use libthreema::protobuf::common::{self, DeltaImage, GroupIdentity, Identities, Unit};
    use libthreema::protobuf::d2d_sync::contact::{
        deprecated_notification_sound_policy_override as dns,
        notification_trigger_policy_override as ntp, read_receipt_policy_override as rrp,
        typing_indicator_policy_override as tip,
    };
    use libthreema::protobuf::d2d_sync::group::{
        deprecated_notification_sound_policy_override as gdns,
        notification_trigger_policy_override as gntp,
    };

    use super::*;

    /// Synthetic IDs (8 characters), each a substring of no other test value.
    const PEER: &str = "A7SYNC01";
    const CREATOR: &str = "A7SYNC02";
    const M1: &str = "A7SYNC03";
    const M2: &str = "A7SYNC04";
    const M3: &str = "A7SYNC05";
    const GID: u64 = 0x0a0b_0c0d_0e0f_1011;

    fn removed() -> Option<DeltaImage> {
        Some(DeltaImage {
            image: Some(common::delta_image::Image::Removed(Unit {})),
        })
    }

    /// A stored contact with EVERY field set (a struct literal with every field named: a field the pinned
    /// proto gains does not compile here, L3).
    fn full_stored() -> d2d_sync::Contact {
        d2d_sync::Contact {
            identity: PEER.to_owned(),
            public_key: Some(vec![1; 32]),
            created_at: Some(1),
            first_name: Some("old first".to_owned()),
            last_name: Some("old last".to_owned()),
            nickname: Some("old nick".to_owned()),
            verification_level: Some(0),
            work_verification_level: Some(0),
            identity_type: Some(0),
            acquaintance_level: Some(0),
            activity_state: Some(0),
            feature_mask: Some(1),
            sync_state: Some(0),
            contact_defined_profile_picture: Some(DeltaImage { image: None }),
            user_defined_profile_picture: Some(DeltaImage { image: None }),
            work_last_full_sync_at: Some(1),
            work_availability_status: Some(d2d_sync::WorkAvailabilityStatus {
                category: 0,
                description: String::new(),
            }),
            read_receipt_policy_override: Some(contact::ReadReceiptPolicyOverride {
                r#override: None,
            }),
            typing_indicator_policy_override: Some(contact::TypingIndicatorPolicyOverride {
                r#override: None,
            }),
            notification_trigger_policy_override: Some(
                contact::NotificationTriggerPolicyOverride { r#override: None },
            ),
            deprecated_notification_sound_policy_override: Some(
                contact::DeprecatedNotificationSoundPolicyOverride { r#override: None },
            ),
            conversation_category: Some(0),
            conversation_visibility: Some(0),
        }
    }

    /// A delta with EVERY field set to another value than [`full_stored`] (every field named).
    fn full_delta() -> d2d_sync::Contact {
        d2d_sync::Contact {
            identity: PEER.to_owned(),
            public_key: Some(vec![2; 32]),
            created_at: Some(2),
            first_name: Some("new first".to_owned()),
            last_name: Some("new last".to_owned()),
            nickname: Some("new nick".to_owned()),
            verification_level: Some(2),
            work_verification_level: Some(1),
            identity_type: Some(1),
            acquaintance_level: Some(1),
            activity_state: Some(1),
            feature_mask: Some(0xff),
            sync_state: Some(1),
            contact_defined_profile_picture: removed(),
            user_defined_profile_picture: removed(),
            work_last_full_sync_at: Some(2),
            work_availability_status: Some(d2d_sync::WorkAvailabilityStatus {
                category: 1,
                description: "busy".to_owned(),
            }),
            read_receipt_policy_override: Some(contact::ReadReceiptPolicyOverride {
                r#override: Some(rrp::Override::Default(Unit {})),
            }),
            typing_indicator_policy_override: Some(contact::TypingIndicatorPolicyOverride {
                r#override: Some(tip::Override::Default(Unit {})),
            }),
            notification_trigger_policy_override: Some(
                contact::NotificationTriggerPolicyOverride {
                    r#override: Some(ntp::Override::Default(Unit {})),
                },
            ),
            deprecated_notification_sound_policy_override: Some(
                contact::DeprecatedNotificationSoundPolicyOverride {
                    r#override: Some(dns::Override::Default(Unit {})),
                },
            ),
            conversation_category: Some(1),
            conversation_visibility: Some(2),
        }
    }

    fn only(identity: &str) -> d2d_sync::Contact {
        d2d_sync::Contact {
            identity: identity.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn every_contact_field_of_a_delta_replaces_the_stored_one_and_an_absent_one_keeps_it() {
        let stored = full_stored();
        let applied = apply_contact(Some(&stored), full_delta()).expect("applied");
        let mut expected = full_delta();
        // H6: the key stays the stored one.
        expected.public_key = stored.public_key.clone();
        assert!(
            applied.contact == expected,
            "a present field did not replace"
        );
        assert!(!applied.created && applied.key_ignored);
        // A delta of the identity alone changes nothing.
        let same = apply_contact(Some(&stored), only(PEER)).expect("applied");
        assert!(
            same.contact == stored,
            "an absent field changed the stored one"
        );
        assert!(!same.key_ignored);
        // Each field alone: only that field moves (the field table, by behaviour).
        let full = full_delta();
        let d2d_sync::Contact {
            identity: _,
            public_key: _,
            created_at,
            first_name,
            last_name,
            nickname,
            verification_level,
            work_verification_level,
            identity_type,
            acquaintance_level,
            activity_state,
            feature_mask,
            sync_state,
            contact_defined_profile_picture,
            user_defined_profile_picture,
            work_last_full_sync_at,
            work_availability_status,
            read_receipt_policy_override,
            typing_indicator_policy_override,
            notification_trigger_policy_override,
            deprecated_notification_sound_policy_override,
            conversation_category,
            conversation_visibility,
        } = full.clone();
        type Set = fn(&mut d2d_sync::Contact, &d2d_sync::Contact);
        let fields: [(&str, Set); 21] = [
            ("created_at", |c, f| c.created_at = f.created_at),
            ("first_name", |c, f| c.first_name.clone_from(&f.first_name)),
            ("last_name", |c, f| c.last_name.clone_from(&f.last_name)),
            ("nickname", |c, f| c.nickname.clone_from(&f.nickname)),
            ("verification_level", |c, f| {
                c.verification_level = f.verification_level;
            }),
            ("work_verification_level", |c, f| {
                c.work_verification_level = f.work_verification_level;
            }),
            ("identity_type", |c, f| c.identity_type = f.identity_type),
            ("acquaintance_level", |c, f| {
                c.acquaintance_level = f.acquaintance_level;
            }),
            ("activity_state", |c, f| c.activity_state = f.activity_state),
            ("feature_mask", |c, f| c.feature_mask = f.feature_mask),
            ("sync_state", |c, f| c.sync_state = f.sync_state),
            ("contact_defined_profile_picture", |c, f| {
                c.contact_defined_profile_picture
                    .clone_from(&f.contact_defined_profile_picture);
            }),
            ("user_defined_profile_picture", |c, f| {
                c.user_defined_profile_picture
                    .clone_from(&f.user_defined_profile_picture);
            }),
            ("work_last_full_sync_at", |c, f| {
                c.work_last_full_sync_at = f.work_last_full_sync_at;
            }),
            ("work_availability_status", |c, f| {
                c.work_availability_status
                    .clone_from(&f.work_availability_status);
            }),
            ("read_receipt_policy_override", |c, f| {
                c.read_receipt_policy_override
                    .clone_from(&f.read_receipt_policy_override);
            }),
            ("typing_indicator_policy_override", |c, f| {
                c.typing_indicator_policy_override
                    .clone_from(&f.typing_indicator_policy_override);
            }),
            ("notification_trigger_policy_override", |c, f| {
                c.notification_trigger_policy_override
                    .clone_from(&f.notification_trigger_policy_override);
            }),
            ("deprecated_notification_sound_policy_override", |c, f| {
                c.deprecated_notification_sound_policy_override
                    .clone_from(&f.deprecated_notification_sound_policy_override);
            }),
            ("conversation_category", |c, f| {
                c.conversation_category = f.conversation_category;
            }),
            ("conversation_visibility", |c, f| {
                c.conversation_visibility = f.conversation_visibility;
            }),
        ];
        // The destructured names are the table's (a field named above and not in the table fails here).
        let named = [
            created_at.is_some(),
            first_name.is_some(),
            last_name.is_some(),
            nickname.is_some(),
            verification_level.is_some(),
            work_verification_level.is_some(),
            identity_type.is_some(),
            acquaintance_level.is_some(),
            activity_state.is_some(),
            feature_mask.is_some(),
            sync_state.is_some(),
            contact_defined_profile_picture.is_some(),
            user_defined_profile_picture.is_some(),
            work_last_full_sync_at.is_some(),
            work_availability_status.is_some(),
            read_receipt_policy_override.is_some(),
            typing_indicator_policy_override.is_some(),
            notification_trigger_policy_override.is_some(),
            deprecated_notification_sound_policy_override.is_some(),
            conversation_category.is_some(),
            conversation_visibility.is_some(),
        ];
        assert_eq!(
            named.len(),
            fields.len(),
            "the field table and the destructuring differ"
        );
        assert!(
            named.iter().all(|n| *n),
            "the full delta leaves a field unset"
        );
        for (name, set) in fields {
            let mut delta = only(PEER);
            set(&mut delta, &full);
            let mut expected = stored.clone();
            set(&mut expected, &full);
            let got = apply_contact(Some(&stored), delta)
                .expect("applied")
                .contact;
            assert!(
                got == expected,
                "the delta of {name} alone was not applied as itself"
            );
        }
    }

    #[test]
    fn h6_the_stored_key_is_kept_and_a_different_one_is_counted() {
        let stored = full_stored();
        let mut delta = only(PEER);
        delta.public_key = Some(vec![9; 32]);
        delta.nickname = Some("kept apart".to_owned());
        let applied = apply_contact(Some(&stored), delta).expect("applied");
        assert!(
            applied.contact.public_key == stored.public_key,
            "the key was replaced"
        );
        assert!(applied.key_ignored, "a different key was not counted");
        assert!(applied.contact.nickname.as_deref() == Some("kept apart"));
        // The same key again: no WARN.
        let mut same = only(PEER);
        same.public_key.clone_from(&stored.public_key);
        assert!(
            !apply_contact(Some(&stored), same)
                .expect("applied")
                .key_ignored
        );
    }

    #[test]
    fn a_new_contact_needs_a_32_byte_key_and_an_invalid_one_is_dropped() {
        // An update of an unknown identity without a key: dropped.
        assert_eq!(
            apply_contact(None, only(PEER)).err(),
            Some(RULE_CONTACT_UNKNOWN)
        );
        let mut short = only(PEER);
        short.public_key = Some(vec![1; 31]);
        assert_eq!(apply_contact(None, short).err(), Some(RULE_CONTACT_KEY));
        let mut new = only(PEER);
        new.public_key = Some(vec![1; 32]);
        new.nickname = Some("n".to_owned());
        let created = apply_contact(None, new.clone()).expect("created");
        assert!(created.created && created.contact == new);
        assert_eq!(
            apply_contact(None, only("SHORT")).err(),
            Some(RULE_CONTACT_INVALID)
        );
        let mut bad = only(PEER);
        bad.acquaintance_level = Some(7);
        assert_eq!(
            apply_contact(Some(&full_stored()), bad).err(),
            Some(RULE_CONTACT_ENUM)
        );
    }

    fn gid() -> Option<GroupIdentity> {
        Some(GroupIdentity {
            group_id: GID,
            creator_identity: CREATOR.to_owned(),
        })
    }

    fn members(ids: &[&str]) -> Option<Identities> {
        Some(Identities {
            identities: ids.iter().map(|i| (*i).to_owned()).collect(),
        })
    }

    /// Every field named (L3).
    fn full_group(name: &str, ids: &[&str], state: i32, visibility: i32) -> d2d_sync::Group {
        d2d_sync::Group {
            group_identity: gid(),
            name: Some(name.to_owned()),
            created_at: Some(u64::try_from(visibility).unwrap_or(0) + 1),
            user_state: Some(state),
            profile_picture: if visibility == 0 {
                Some(DeltaImage { image: None })
            } else {
                removed()
            },
            member_identities: members(ids),
            notification_trigger_policy_override: Some(group::NotificationTriggerPolicyOverride {
                r#override: (visibility != 0).then_some(gntp::Override::Default(Unit {})),
            }),
            deprecated_notification_sound_policy_override: Some(
                group::DeprecatedNotificationSoundPolicyOverride {
                    r#override: (visibility != 0).then_some(gdns::Override::Default(Unit {})),
                },
            ),
            conversation_category: Some(visibility.min(1)),
            conversation_visibility: Some(visibility),
        }
    }

    fn group_only() -> d2d_sync::Group {
        d2d_sync::Group {
            group_identity: gid(),
            ..Default::default()
        }
    }

    #[test]
    fn every_group_field_of_a_delta_replaces_the_stored_one_and_members_are_replaced_not_appended()
    {
        let stored = full_group("old", &[M1, M2, M3], 0, 0);
        let delta = full_group("new", &[M1, M2], 1, 2);
        let applied = apply_group(Some(&stored), delta.clone()).expect("applied");
        assert!(
            applied.group == delta,
            "a present group field did not replace"
        );
        assert_eq!(applied.title.as_deref(), Some("new"));
        assert!(applied.picture && !applied.created);
        // Members 3 -> 2, never 5 (prost's merge would append).
        let mut fewer = group_only();
        fewer.member_identities = members(&[M2, M1]);
        let got = apply_group(Some(&stored), fewer).expect("applied").group;
        assert_eq!(
            got.member_identities.map(|m| m.identities),
            Some(vec![M2.to_owned(), M1.to_owned()]),
            "the member list was not replaced whole"
        );
        // Absent: unchanged, no title, no picture.
        let same = apply_group(Some(&stored), group_only()).expect("applied");
        assert!(same.group == stored && same.title.is_none() && !same.picture);
        // The same name again: no title change.
        let mut renamed = group_only();
        renamed.name = Some("old".to_owned());
        assert!(
            apply_group(Some(&stored), renamed)
                .expect("applied")
                .title
                .is_none()
        );
    }

    #[test]
    fn a_group_delta_is_validated_and_bounded() {
        let mut no_id = group_only();
        no_id.group_identity = None;
        assert_eq!(apply_group(None, no_id).err(), Some(RULE_GROUP_INVALID));
        let mut bad_member = group_only();
        bad_member.member_identities = members(&["SHORT"]);
        assert_eq!(apply_group(None, bad_member).err(), Some(RULE_GROUP_MEMBER));
        let mut bad_state = group_only();
        bad_state.user_state = Some(9);
        assert_eq!(apply_group(None, bad_state).err(), Some(RULE_GROUP_ENUM));
        let many: Vec<String> = (0..=MAX_MEMBERS).map(|i| format!("A7{i:06}")).collect();
        let mut at = group_only();
        at.member_identities = Some(Identities {
            identities: many[..MAX_MEMBERS].to_vec(),
        });
        assert!(apply_group(None, at).is_ok(), "N members are taken");
        let mut over = group_only();
        over.member_identities = Some(Identities { identities: many });
        assert_eq!(apply_group(None, over).err(), Some(RULE_OVER_BOUND));
    }

    #[test]
    fn m1_an_empty_name_clears_and_a_left_group_without_members_is_not_visible() {
        let mut cleared = full_stored();
        cleared.first_name = Some(String::new());
        cleared.last_name = Some(String::new());
        cleared.nickname = Some(String::new());
        let Ok(BackendEvent::ContactUpsert(patch)) =
            events::contact_event(&cleared, &std::collections::HashSet::new())
        else {
            panic!("a contact event");
        };
        assert!(
            patch.profile_name == Patch::Clear,
            "an empty nickname clears"
        );
        assert!(patch.system_name == Patch::Clear, "empty names clear");
        let group = |sync| SnapGroup {
            creator: CREATOR.to_owned(),
            group_id: GID,
            sync,
        };
        let mut nameless = group_only();
        nameless.name = Some(String::new());
        let patch: Option<ConversationPatch> =
            events::group_patch(&group(nameless), PEER).expect("a patch");
        assert!(
            patch.is_some_and(|p| p.title == Patch::Clear),
            "an empty title clears"
        );
        let mut left = full_group("g", &[], group::UserState::Left as i32, 0);
        assert!(
            events::group_patch(&group(left.clone()), PEER)
                .expect("ok")
                .is_none()
        );
        left.member_identities = members(&[M1]);
        assert!(
            events::group_patch(&group(left), PEER)
                .expect("ok")
                .is_some()
        );
    }

    /// A file that holds one stored group.
    struct OneGroup(d2d_sync::Group);

    impl View for OneGroup {
        fn contact(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        fn group(&self, _: &str, _: u64) -> Result<bool> {
            Ok(true)
        }

        fn stored_contact(&self, _: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(None)
        }

        fn stored_group(&self, _: &str, _: u64) -> Result<Option<d2d_sync::Group>> {
            Ok(Some(self.0.clone()))
        }

        fn blocked(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        fn sync_counts(&self) -> Result<(u64, u64)> {
            Ok((0, 1))
        }

        crate::reflect::map::no_b4_rows!();
    }

    fn update(group: d2d_sync::Group) -> d2d::GroupSync {
        d2d::GroupSync {
            action: Some(d2d::group_sync::Action::Update(d2d::group_sync::Update {
                group: Some(group),
                member_state_changes: HashMap::new(),
            })),
        }
    }

    /// M-1 (round A): an Update that makes a VISIBLE group not visible (M1: LEFT, nobody listed) writes the
    /// row and states NOTHING: `ConversationRemoved` would delete the chat's history on the core's side while
    /// the phone keeps it. The same update that leaves a member states the group and the notice. Only
    /// `GroupSync.Delete` removes the chat.
    #[test]
    fn m_1_a_group_that_becomes_not_visible_states_nothing_and_only_a_delete_removes_it() {
        const OWN: &str = "A7SYNC06";
        let cx = Cx {
            own: OWN,
            now_ms: 1,
            server_at: 1,
            catch_up: false,
            notice_tag: [0; 8],
            remap: false,
        };
        let stored = full_group("g", &[M1], group::UserState::Member as i32, 0);
        let view = OneGroup(stored);
        let left = |ids: &[&str]| d2d_sync::Group {
            group_identity: gid(),
            user_state: Some(group::UserState::Left as i32),
            member_identities: members(ids),
            ..Default::default()
        };
        let hidden = group_sync(&update(left(&[])), &cx, &view).expect("maps");
        assert!(
            hidden.events.is_empty() && hidden.dropped.is_none() && hidden.deferred.is_none(),
            "a group that is not visible any more stated something: {:?}",
            hidden.events
        );
        assert!(
            matches!(hidden.writes.as_slice(), [Write::Group { .. }]),
            "the row of the group was not written"
        );
        let shown = group_sync(&update(left(&[M1])), &cx, &view).expect("maps");
        assert!(
            matches!(
                shown.events.first(),
                Some(BackendEvent::ConversationUpsert(_))
            ),
            "a group that stays visible did not state its patch"
        );
        let delete = d2d::GroupSync {
            action: Some(d2d::group_sync::Action::Delete(d2d::group_sync::Delete {
                group_identity: gid(),
            })),
        };
        let deleted = group_sync(&delete, &cx, &view).expect("maps");
        assert!(
            matches!(
                deleted.events.as_slice(),
                [BackendEvent::ConversationRemoved { .. }]
            ),
            "GroupSync.Delete does not remove the chat"
        );
    }

    // TH5b B4 (R13): THE SETTINGS, THE PROFILE AND THE LISTS ---------------------------------------------

    use kourier_model::{AccountSettingsPatch, ContactPatch, Redacted, UserId};
    use libthreema::protobuf::d2d_sync::user_profile;

    /// Synthetic IDs of B4 (8 characters), each a substring of no other test value.
    const OWN_B4: &str = "B4SYNC00";
    const A: &str = "B4SYNC0A";
    const B: &str = "B4SYNC0B";
    const C: &str = "B4SYNC0C";
    const D: &str = "B4SYNC0D";
    const STRANGER: &str = "B4SYNC0S";
    const LIST: u64 = 0x0b04_0000_0000_0001;

    fn ids(list: &[&str]) -> Option<Identities> {
        Some(Identities {
            identities: list.iter().map(|i| (*i).to_owned()).collect(),
        })
    }

    /// Stored settings with EVERY field set (a struct literal with every field named, L3).
    fn full_settings() -> d2d_sync::Settings {
        d2d_sync::Settings {
            deprecated_contact_sync_policy: Some(0),
            unknown_contact_policy: Some(0),
            read_receipt_policy: Some(0),
            typing_indicator_policy: Some(0),
            o2o_call_policy: Some(0),
            o2o_call_connection_policy: Some(0),
            o2o_call_video_policy: Some(0),
            group_call_policy: Some(0),
            screenshot_policy: Some(0),
            deprecated_keyboard_data_collection_policy: Some(0),
            blocked_identities: ids(&[A, B]),
            exclude_from_sync_identities: ids(&[C]),
        }
    }

    /// A delta with EVERY field set to another value than [`full_settings`] (every field named).
    fn full_settings_delta() -> d2d_sync::Settings {
        d2d_sync::Settings {
            deprecated_contact_sync_policy: Some(1),
            unknown_contact_policy: Some(1),
            read_receipt_policy: Some(1),
            typing_indicator_policy: Some(1),
            o2o_call_policy: Some(1),
            o2o_call_connection_policy: Some(1),
            o2o_call_video_policy: Some(1),
            group_call_policy: Some(1),
            screenshot_policy: Some(1),
            deprecated_keyboard_data_collection_policy: Some(1),
            blocked_identities: ids(&[C]),
            exclude_from_sync_identities: ids(&[]),
        }
    }

    #[test]
    fn every_settings_field_of_a_delta_replaces_the_stored_one_and_the_id_lists_are_replaced_not_appended()
     {
        let stored = full_settings();
        let applied = apply_settings(Some(&stored), full_settings_delta()).expect("applied");
        assert!(
            applied == full_settings_delta(),
            "a present settings field did not replace"
        );
        // The blocked list [A, B] -> [C], never [A, B, C] (prost's merge appends); the excluded list [C] -> [].
        assert_eq!(
            applied.blocked_identities.map(|b| b.identities),
            Some(vec![C.to_owned()]),
            "the blocked list was not replaced whole"
        );
        assert_eq!(
            applied.exclude_from_sync_identities.map(|b| b.identities),
            Some(Vec::<String>::new()),
            "an empty excluded list is a value (the spec: valid)"
        );
        // An empty delta changes nothing; a file without settings takes the delta as it is.
        assert!(
            apply_settings(Some(&stored), d2d_sync::Settings::default()).expect("applied")
                == stored,
            "an absent field changed the stored one"
        );
        assert!(
            apply_settings(None, full_settings_delta()).expect("applied") == full_settings_delta(),
            "a file without settings did not take the delta"
        );
        // Each field alone: only that field moves (the field table, by behaviour).
        let full = full_settings_delta();
        let d2d_sync::Settings {
            deprecated_contact_sync_policy,
            unknown_contact_policy,
            read_receipt_policy,
            typing_indicator_policy,
            o2o_call_policy,
            o2o_call_connection_policy,
            o2o_call_video_policy,
            group_call_policy,
            screenshot_policy,
            deprecated_keyboard_data_collection_policy,
            blocked_identities,
            exclude_from_sync_identities,
        } = full.clone();
        type Set = fn(&mut d2d_sync::Settings, &d2d_sync::Settings);
        let fields: [(&str, Set); 12] = [
            ("deprecated_contact_sync_policy", |s, f| {
                s.deprecated_contact_sync_policy = f.deprecated_contact_sync_policy;
            }),
            ("unknown_contact_policy", |s, f| {
                s.unknown_contact_policy = f.unknown_contact_policy;
            }),
            ("read_receipt_policy", |s, f| {
                s.read_receipt_policy = f.read_receipt_policy;
            }),
            ("typing_indicator_policy", |s, f| {
                s.typing_indicator_policy = f.typing_indicator_policy;
            }),
            ("o2o_call_policy", |s, f| {
                s.o2o_call_policy = f.o2o_call_policy
            }),
            ("o2o_call_connection_policy", |s, f| {
                s.o2o_call_connection_policy = f.o2o_call_connection_policy;
            }),
            ("o2o_call_video_policy", |s, f| {
                s.o2o_call_video_policy = f.o2o_call_video_policy;
            }),
            ("group_call_policy", |s, f| {
                s.group_call_policy = f.group_call_policy;
            }),
            ("screenshot_policy", |s, f| {
                s.screenshot_policy = f.screenshot_policy;
            }),
            ("deprecated_keyboard_data_collection_policy", |s, f| {
                s.deprecated_keyboard_data_collection_policy =
                    f.deprecated_keyboard_data_collection_policy;
            }),
            ("blocked_identities", |s, f| {
                s.blocked_identities.clone_from(&f.blocked_identities);
            }),
            ("exclude_from_sync_identities", |s, f| {
                s.exclude_from_sync_identities
                    .clone_from(&f.exclude_from_sync_identities);
            }),
        ];
        let named = [
            deprecated_contact_sync_policy.is_some(),
            unknown_contact_policy.is_some(),
            read_receipt_policy.is_some(),
            typing_indicator_policy.is_some(),
            o2o_call_policy.is_some(),
            o2o_call_connection_policy.is_some(),
            o2o_call_video_policy.is_some(),
            group_call_policy.is_some(),
            screenshot_policy.is_some(),
            deprecated_keyboard_data_collection_policy.is_some(),
            blocked_identities.is_some(),
            exclude_from_sync_identities.is_some(),
        ];
        assert_eq!(
            named.len(),
            fields.len(),
            "the field table and the destructuring differ"
        );
        assert!(
            named.iter().all(|n| *n),
            "the full delta leaves a field unset"
        );
        for (name, set) in fields {
            let mut delta = d2d_sync::Settings::default();
            set(&mut delta, &full);
            let mut expected = stored.clone();
            set(&mut expected, &full);
            let got = apply_settings(Some(&stored), delta).expect("applied");
            assert!(
                got == expected,
                "the delta of {name} alone was not applied as itself"
            );
        }
    }

    #[test]
    fn a_settings_delta_is_validated_and_bounded() {
        // Every enum field with a value the pinned proto lacks: dropped, whichever field.
        type Set = fn(&mut d2d_sync::Settings);
        let bad: [Set; 10] = [
            |s| s.deprecated_contact_sync_policy = Some(2),
            |s| s.unknown_contact_policy = Some(2),
            |s| s.read_receipt_policy = Some(2),
            |s| s.typing_indicator_policy = Some(2),
            |s| s.o2o_call_policy = Some(2),
            |s| s.o2o_call_connection_policy = Some(2),
            |s| s.o2o_call_video_policy = Some(2),
            |s| s.group_call_policy = Some(2),
            |s| s.screenshot_policy = Some(2),
            |s| s.deprecated_keyboard_data_collection_policy = Some(-1),
        ];
        for (i, set) in bad.into_iter().enumerate() {
            let mut delta = d2d_sync::Settings::default();
            set(&mut delta);
            assert_eq!(
                apply_settings(Some(&full_settings()), delta).err(),
                Some(RULE_SETTINGS_ENUM),
                "enum field {i}"
            );
        }
        // An entry that is no Threema ID, in either list.
        let blocked = d2d_sync::Settings {
            blocked_identities: ids(&[A, "SHORT"]),
            ..Default::default()
        };
        assert_eq!(
            apply_settings(None, blocked).err(),
            Some(RULE_SETTINGS_INVALID)
        );
        let excluded = d2d_sync::Settings {
            exclude_from_sync_identities: ids(&["lower123"]),
            ..Default::default()
        };
        assert_eq!(
            apply_settings(None, excluded).err(),
            Some(RULE_SETTINGS_INVALID)
        );
        // TH4's bound: 100,000 IDs pass, 100,001 are dropped, in either list.
        let many: Vec<String> = (0..=MAX_IDENTITIES).map(|i| format!("B4{i:06}")).collect();
        let list = |n: usize| {
            Some(Identities {
                identities: many[..n].to_vec(),
            })
        };
        for field in 0..2 {
            let with = |n: usize| {
                let mut delta = d2d_sync::Settings::default();
                if field == 0 {
                    delta.blocked_identities = list(n);
                } else {
                    delta.exclude_from_sync_identities = list(n);
                }
                delta
            };
            assert!(
                apply_settings(None, with(MAX_IDENTITIES)).is_ok(),
                "N IDs pass"
            );
            assert_eq!(
                apply_settings(None, with(MAX_IDENTITIES + 1)).err(),
                Some(RULE_OVER_BOUND),
                "N+1 IDs ({field})"
            );
        }
    }

    /// A test file of B4: settings, a profile, contacts and lists.
    #[derive(Default)]
    struct Rows {
        settings: Option<d2d_sync::Settings>,
        profile: Option<d2d_sync::UserProfile>,
        contacts: Vec<d2d_sync::Contact>,
        lists: Vec<d2d_sync::DistributionList>,
        /// How many lists the file says it holds (at least `lists.len()`).
        list_count: u64,
    }

    impl View for Rows {
        fn contact(&self, identity: &str) -> Result<bool> {
            Ok(self.contacts.iter().any(|c| c.identity == identity))
        }

        fn group(&self, _: &str, _: u64) -> Result<bool> {
            Ok(false)
        }

        fn stored_contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(self
                .contacts
                .iter()
                .find(|c| c.identity == identity)
                .cloned())
        }

        fn stored_group(&self, _: &str, _: u64) -> Result<Option<d2d_sync::Group>> {
            Ok(None)
        }

        fn blocked(&self, identity: &str) -> Result<bool> {
            Ok(blocked_of(self.settings.as_ref()).contains(identity))
        }

        fn sync_counts(&self) -> Result<(u64, u64)> {
            Ok((u64::try_from(self.contacts.len()).unwrap_or(u64::MAX), 0))
        }

        fn stored_settings(&self) -> Result<Option<d2d_sync::Settings>> {
            Ok(self.settings.clone())
        }

        fn stored_profile(&self) -> Result<Option<d2d_sync::UserProfile>> {
            Ok(self.profile.clone())
        }

        fn stored_list(&self, id: u64) -> Result<Option<d2d_sync::DistributionList>> {
            Ok(self
                .lists
                .iter()
                .find(|l| l.distribution_list_id == id)
                .cloned())
        }

        fn list_count(&self) -> Result<u64> {
            Ok(self
                .list_count
                .max(u64::try_from(self.lists.len()).unwrap_or(u64::MAX)))
        }
    }

    fn b4_user(id: &str) -> UserId {
        UserId::from_parts("threema", id).expect("a user")
    }

    /// A stored contact's `ContactUpsert` as the core gets it, written out: no names, direct, `blocked`.
    fn upsert(id: &str, blocked: bool) -> BackendEvent {
        let mut patch = ContactPatch::new(b4_user(id));
        patch.hidden = Some(false);
        patch.blocked = Some(blocked);
        BackendEvent::ContactUpsert(patch)
    }

    fn switches(read_receipts: Option<bool>, typing: Option<bool>) -> BackendEvent {
        BackendEvent::AccountSettings(AccountSettingsPatch {
            read_receipts,
            typing_indicators: typing,
            ..AccountSettingsPatch::default()
        })
    }

    fn settings_update(settings: d2d_sync::Settings) -> d2d::SettingsSync {
        d2d::SettingsSync {
            action: Some(d2d::settings_sync::Action::Update(
                d2d::settings_sync::Update {
                    settings: Some(settings),
                },
            )),
        }
    }

    /// The stored settings a `Write::Settings` carries.
    fn written_settings(mapped: &Mapped) -> &d2d_sync::Settings {
        match mapped.writes.as_slice() {
            [Write::Settings(s)] => s,
            other => panic!("not one settings write: {other:?}"),
        }
    }

    /// R13: a settings delta states `AccountSettings` of the whole new message and the `ContactUpsert` of every
    /// STORED contact whose blocked state changed, by ID: B was blocked and is not (unblocked), A was not and
    /// is (blocked), D stays blocked and C stays unblocked (NO event), the stranger S has no contact (nothing).
    #[test]
    fn a_settings_sync_states_the_switches_and_every_stored_contact_whose_blocked_state_changed() {
        let contact = |id: &str| d2d_sync::Contact {
            identity: id.to_owned(),
            public_key: Some(vec![4; 32]),
            ..Default::default()
        };
        let rows = Rows {
            settings: Some(d2d_sync::Settings {
                read_receipt_policy: Some(d2d_sync::ReadReceiptPolicy::DontSendReadReceipt as i32),
                typing_indicator_policy: Some(
                    d2d_sync::TypingIndicatorPolicy::SendTypingIndicator as i32,
                ),
                blocked_identities: ids(&[D, B, STRANGER]),
                ..Default::default()
            }),
            contacts: [A, B, C, D].into_iter().map(contact).collect(),
            ..Rows::default()
        };
        let delta = d2d_sync::Settings {
            read_receipt_policy: Some(d2d_sync::ReadReceiptPolicy::SendReadReceipt as i32),
            typing_indicator_policy: Some(
                d2d_sync::TypingIndicatorPolicy::DontSendTypingIndicator as i32,
            ),
            blocked_identities: ids(&[D, A]),
            ..Default::default()
        };
        let mapped = settings_sync(&settings_update(delta), &rows).expect("maps");
        assert!(
            mapped.events
                == vec![
                    switches(Some(true), Some(false)),
                    upsert(A, true),
                    upsert(B, false),
                ],
            "the settings delta's events differ from the literal ones"
        );
        assert!(mapped.dropped.is_none() && mapped.deferred.is_none());
        assert_eq!(
            written_settings(&mapped)
                .blocked_identities
                .clone()
                .map(|b| b.identities),
            Some(vec![D.to_owned(), A.to_owned()]),
            "the stored blocked list is not the delta's, in its order"
        );
        // A delta that leaves the blocked list: the switches of the whole message alone (the stored ones).
        let screenshot = d2d_sync::Settings {
            screenshot_policy: Some(1),
            ..Default::default()
        };
        let mapped = settings_sync(&settings_update(screenshot), &rows).expect("maps");
        assert!(
            mapped.events == vec![switches(Some(false), Some(true))],
            "a delta without a blocked list stated a contact"
        );
        // A file without settings: nothing was blocked; the switches it does not know are not stated.
        let fresh = Rows {
            contacts: vec![contact(A)],
            ..Rows::default()
        };
        let mapped = settings_sync(
            &settings_update(d2d_sync::Settings {
                blocked_identities: ids(&[A]),
                ..Default::default()
            }),
            &fresh,
        )
        .expect("maps");
        assert!(
            mapped.events == vec![switches(None, None), upsert(A, true)],
            "a first settings row's events differ"
        );
        // A refused delta writes and states nothing; a sync without its message is `sync-empty`.
        let refused = settings_sync(
            &settings_update(d2d_sync::Settings {
                screenshot_policy: Some(7),
                ..Default::default()
            }),
            &rows,
        )
        .expect("maps");
        assert!(
            refused.dropped == Some(RULE_SETTINGS_ENUM)
                && refused.events.is_empty()
                && refused.writes.is_empty()
        );
        for empty in [
            d2d::SettingsSync { action: None },
            d2d::SettingsSync {
                action: Some(d2d::settings_sync::Action::Update(
                    d2d::settings_sync::Update { settings: None },
                )),
            },
        ] {
            let got = settings_sync(&empty, &rows).expect("maps");
            assert!(got.dropped == Some(RULE_SYNC_EMPTY) && got.writes.is_empty());
        }
    }

    /// A stored profile with EVERY field set (every field named, L3).
    fn full_profile() -> d2d_sync::UserProfile {
        d2d_sync::UserProfile {
            nickname: Some("old own nick".to_owned()),
            profile_picture: Some(DeltaImage { image: None }),
            profile_picture_share_with: Some(user_profile::ProfilePictureShareWith {
                policy: Some(user_profile::profile_picture_share_with::Policy::Nobody(
                    Unit {},
                )),
            }),
            identity_links: Some(user_profile::IdentityLinks { links: Vec::new() }),
            work_availability_status: Some(d2d_sync::WorkAvailabilityStatus {
                category: 0,
                description: String::new(),
            }),
        }
    }

    /// A delta with EVERY field set to another value than [`full_profile`] (every field named).
    fn full_profile_delta() -> d2d_sync::UserProfile {
        d2d_sync::UserProfile {
            nickname: Some("new own nick".to_owned()),
            profile_picture: removed(),
            profile_picture_share_with: Some(user_profile::ProfilePictureShareWith {
                policy: Some(user_profile::profile_picture_share_with::Policy::AllowList(
                    Identities {
                        identities: vec![A.to_owned()],
                    },
                )),
            }),
            identity_links: Some(user_profile::IdentityLinks {
                links: vec![user_profile::identity_links::IdentityLink {
                    r#type: Some(user_profile::identity_links::identity_link::Type::Email(
                        "synthetic@example.invalid".to_owned(),
                    )),
                    description: String::new(),
                }],
            }),
            work_availability_status: Some(d2d_sync::WorkAvailabilityStatus {
                category: 2,
                description: "synthetic".to_owned(),
            }),
        }
    }

    #[test]
    fn every_profile_field_of_a_delta_replaces_the_stored_one_and_an_absent_one_keeps_it() {
        let stored = full_profile();
        assert!(
            apply_profile(Some(&stored), full_profile_delta()) == full_profile_delta(),
            "a present profile field did not replace"
        );
        assert!(
            apply_profile(Some(&stored), d2d_sync::UserProfile::default()) == stored,
            "an absent profile field changed the stored one"
        );
        assert!(apply_profile(None, full_profile_delta()) == full_profile_delta());
        let full = full_profile_delta();
        let d2d_sync::UserProfile {
            nickname,
            profile_picture,
            profile_picture_share_with,
            identity_links,
            work_availability_status,
        } = full.clone();
        type Set = fn(&mut d2d_sync::UserProfile, &d2d_sync::UserProfile);
        let fields: [(&str, Set); 5] = [
            ("nickname", |p, f| p.nickname.clone_from(&f.nickname)),
            ("profile_picture", |p, f| {
                p.profile_picture.clone_from(&f.profile_picture);
            }),
            ("profile_picture_share_with", |p, f| {
                p.profile_picture_share_with
                    .clone_from(&f.profile_picture_share_with);
            }),
            ("identity_links", |p, f| {
                p.identity_links.clone_from(&f.identity_links);
            }),
            ("work_availability_status", |p, f| {
                p.work_availability_status
                    .clone_from(&f.work_availability_status);
            }),
        ];
        let named = [
            nickname.is_some(),
            profile_picture.is_some(),
            profile_picture_share_with.is_some(),
            identity_links.is_some(),
            work_availability_status.is_some(),
        ];
        assert_eq!(named.len(), fields.len());
        assert!(named.iter().all(|n| *n));
        for (name, set) in fields {
            let mut delta = d2d_sync::UserProfile::default();
            set(&mut delta, &full);
            let mut expected = stored.clone();
            set(&mut expected, &full);
            assert!(
                apply_profile(Some(&stored), delta) == expected,
                "the delta of {name} alone was not applied as itself"
            );
        }
    }

    fn ready(display_name: Option<&str>) -> BackendEvent {
        BackendEvent::AccountReady {
            own_ids: vec![b4_user(OWN_B4)],
            device: None,
            display_name: display_name.map(|n| Redacted::new(n.to_owned())),
            number: None,
        }
    }

    fn profile_update(profile: d2d_sync::UserProfile) -> d2d::UserProfileSync {
        d2d::UserProfileSync {
            action: Some(d2d::user_profile_sync::Action::Update(
                d2d::user_profile_sync::Update {
                    user_profile: Some(profile),
                },
            )),
        }
    }

    fn b4_cx() -> Cx<'static> {
        Cx {
            own: OWN_B4,
            now_ms: 1,
            server_at: 1,
            catch_up: false,
            notice_tag: [0; 8],
            remap: false,
        }
    }

    /// R13 + F5: a profile delta states `AccountReady` with the whole new message's nickname; an EMPTY
    /// nickname clears the stored one and states `display_name: None` (the core keeps its old name: the
    /// contract's limit); a picture-only delta restates the stored nickname.
    #[test]
    fn a_profile_sync_states_account_ready_with_the_nickname_and_an_empty_one_clears_the_stored_one()
     {
        let rows = Rows {
            profile: Some(full_profile()),
            ..Rows::default()
        };
        let nick = d2d_sync::UserProfile {
            nickname: Some("Synthetic Own".to_owned()),
            ..Default::default()
        };
        let mapped = profile_sync(&profile_update(nick), &b4_cx(), &rows).expect("maps");
        assert!(mapped.events == vec![ready(Some("Synthetic Own"))]);
        let cleared = d2d_sync::UserProfile {
            nickname: Some(String::new()),
            ..Default::default()
        };
        let mapped = profile_sync(&profile_update(cleared), &b4_cx(), &rows).expect("maps");
        assert!(
            mapped.events == vec![ready(None)],
            "an empty nickname states a name"
        );
        match mapped.writes.as_slice() {
            [Write::Profile(p)] => {
                assert_eq!(
                    p.nickname.as_deref(),
                    Some(""),
                    "the empty nickname is not stored as-is"
                );
                assert!(
                    p.profile_picture == full_profile().profile_picture,
                    "an absent field moved"
                );
            }
            other => panic!("not one profile write: {other:?}"),
        }
        let picture = d2d_sync::UserProfile {
            profile_picture: removed(),
            ..Default::default()
        };
        let mapped = profile_sync(&profile_update(picture), &b4_cx(), &rows).expect("maps");
        assert!(
            mapped.events == vec![ready(Some("old own nick"))],
            "a picture-only delta did not restate the stored nickname"
        );
        // A file without a profile row: the delta is the profile.
        let mapped = profile_sync(
            &profile_update(d2d_sync::UserProfile::default()),
            &b4_cx(),
            &Rows::default(),
        )
        .expect("maps");
        assert!(mapped.events == vec![ready(None)]);
        let empty =
            profile_sync(&d2d::UserProfileSync { action: None }, &b4_cx(), &rows).expect("maps");
        assert!(empty.dropped == Some(RULE_SYNC_EMPTY) && empty.writes.is_empty());
    }

    /// A stored list with EVERY field set (every field named, L3).
    fn full_list() -> d2d_sync::DistributionList {
        d2d_sync::DistributionList {
            distribution_list_id: LIST,
            name: Some("old list".to_owned()),
            created_at: Some(1),
            member_identities: ids(&[A, B, C]),
            conversation_category: Some(0),
            conversation_visibility: Some(0),
        }
    }

    fn full_list_delta() -> d2d_sync::DistributionList {
        d2d_sync::DistributionList {
            distribution_list_id: LIST,
            name: Some(String::new()),
            created_at: Some(2),
            member_identities: ids(&[D]),
            conversation_category: Some(1),
            conversation_visibility: Some(2),
        }
    }

    fn list_only() -> d2d_sync::DistributionList {
        d2d_sync::DistributionList {
            distribution_list_id: LIST,
            ..Default::default()
        }
    }

    #[test]
    fn every_list_field_of_a_delta_replaces_the_stored_one_and_members_are_replaced_not_appended() {
        let stored = full_list();
        assert!(
            apply_list(Some(&stored), full_list_delta()).expect("applied") == full_list_delta(),
            "a present list field did not replace (an empty name is a value)"
        );
        assert!(apply_list(Some(&stored), list_only()).expect("applied") == stored);
        let mut fewer = list_only();
        fewer.member_identities = ids(&[C, A]);
        assert_eq!(
            apply_list(Some(&stored), fewer)
                .expect("applied")
                .member_identities
                .map(|m| m.identities),
            Some(vec![C.to_owned(), A.to_owned()]),
            "the member list was not replaced whole"
        );
        let full = full_list_delta();
        let d2d_sync::DistributionList {
            distribution_list_id: _,
            name,
            created_at,
            member_identities,
            conversation_category,
            conversation_visibility,
        } = full.clone();
        type Set = fn(&mut d2d_sync::DistributionList, &d2d_sync::DistributionList);
        let fields: [(&str, Set); 5] = [
            ("name", |l, f| l.name.clone_from(&f.name)),
            ("created_at", |l, f| l.created_at = f.created_at),
            ("member_identities", |l, f| {
                l.member_identities.clone_from(&f.member_identities);
            }),
            ("conversation_category", |l, f| {
                l.conversation_category = f.conversation_category;
            }),
            ("conversation_visibility", |l, f| {
                l.conversation_visibility = f.conversation_visibility;
            }),
        ];
        let named = [
            name.is_some(),
            created_at.is_some(),
            member_identities.is_some(),
            conversation_category.is_some(),
            conversation_visibility.is_some(),
        ];
        assert_eq!(named.len(), fields.len());
        assert!(named.iter().all(|n| *n));
        for (field, set) in fields {
            let mut delta = list_only();
            set(&mut delta, &full);
            let mut expected = stored.clone();
            set(&mut expected, &full);
            assert!(
                apply_list(Some(&stored), delta).expect("applied") == expected,
                "the delta of {field} alone was not applied as itself"
            );
        }
    }

    #[test]
    fn a_list_delta_is_validated_and_bounded() {
        let stored = full_list();
        // R13: an EMPTY member list is dropped (the spec: not valid); so is a non-ID and a new list without
        // members.
        let mut empty = list_only();
        empty.member_identities = ids(&[]);
        assert_eq!(
            apply_list(Some(&stored), empty).err(),
            Some(RULE_LIST_INVALID)
        );
        let mut bad = list_only();
        bad.member_identities = ids(&[A, "SHORT"]);
        assert_eq!(
            apply_list(Some(&stored), bad).err(),
            Some(RULE_LIST_INVALID)
        );
        assert_eq!(apply_list(None, list_only()).err(), Some(RULE_LIST_INVALID));
        let mut new = list_only();
        new.member_identities = ids(&[A]);
        assert!(apply_list(None, new.clone()).expect("created") == new);
        // A stored list of another id (a belt).
        let mut other = full_list();
        other.distribution_list_id = LIST + 1;
        assert_eq!(
            apply_list(Some(&other), list_only()).err(),
            Some(RULE_LIST_INVALID)
        );
        // The strict enums.
        for set in [
            |l: &mut d2d_sync::DistributionList| l.conversation_category = Some(2),
            |l: &mut d2d_sync::DistributionList| l.conversation_visibility = Some(3),
        ] {
            let mut delta = list_only();
            set(&mut delta);
            assert_eq!(apply_list(Some(&stored), delta).err(), Some(RULE_LIST_ENUM));
        }
        // TH4's bound: 10,000 members pass, 10,001 are dropped.
        let many: Vec<String> = (0..=MAX_MEMBERS).map(|i| format!("B4{i:06}")).collect();
        let mut at = list_only();
        at.member_identities = Some(Identities {
            identities: many[..MAX_MEMBERS].to_vec(),
        });
        assert!(apply_list(None, at).is_ok(), "N members are taken");
        let mut over = list_only();
        over.member_identities = Some(Identities { identities: many });
        assert_eq!(apply_list(None, over).err(), Some(RULE_OVER_BOUND));
    }

    fn list_action(action: d2d::distribution_list_sync::Action) -> d2d::DistributionListSync {
        d2d::DistributionListSync {
            action: Some(action),
        }
    }

    fn create(list: d2d_sync::DistributionList) -> d2d::DistributionListSync {
        list_action(d2d::distribution_list_sync::Action::Create(
            d2d::distribution_list_sync::Create {
                distribution_list: Some(list),
            },
        ))
    }

    fn list_update(list: d2d_sync::DistributionList) -> d2d::DistributionListSync {
        list_action(d2d::distribution_list_sync::Action::Update(
            d2d::distribution_list_sync::Update {
                distribution_list: Some(list),
            },
        ))
    }

    fn delete(id: u64) -> d2d::DistributionListSync {
        list_action(d2d::distribution_list_sync::Action::Delete(
            d2d::distribution_list_sync::Delete {
                distribution_list_id: id,
            },
        ))
    }

    /// R13: a list Create / Update / Delete writes its row and states NOTHING; a Create of a held list is an
    /// update; an Update or Delete of an unknown list is `list-unknown`; a NEW list past 10,000 is `over-bound`
    /// (a held one is still updated); a sync without its message is `sync-empty`.
    #[test]
    fn a_list_sync_creates_updates_and_deletes_its_row_and_states_nothing() {
        let held = Rows {
            lists: vec![full_list()],
            ..Rows::default()
        };
        let quiet = |m: &Mapped| m.events.is_empty() && m.dropped.is_none() && m.deferred.is_none();
        let written = |m: &Mapped| match m.writes.as_slice() {
            [Write::List(l)] => (**l).clone(),
            other => panic!("not one list write: {other:?}"),
        };
        let mut new = list_only();
        new.member_identities = ids(&[B]);
        new.name = Some("synthetic list".to_owned());
        let created = list_sync(&create(new.clone()), &Rows::default()).expect("maps");
        assert!(quiet(&created) && written(&created) == new, "a create");
        let mut renamed = list_only();
        renamed.name = Some("renamed".to_owned());
        let again = list_sync(&create(renamed.clone()), &held).expect("maps");
        let mut expected = full_list();
        expected.name = Some("renamed".to_owned());
        assert!(
            quiet(&again) && written(&again) == expected,
            "a create of a held list is an update"
        );
        let updated = list_sync(&list_update(renamed.clone()), &held).expect("maps");
        assert!(
            quiet(&updated) && written(&updated) == expected,
            "an update"
        );
        let deleted = list_sync(&delete(LIST), &held).expect("maps");
        assert!(
            quiet(&deleted)
                && matches!(deleted.writes.as_slice(), [Write::ListDeleted { id }] if *id == LIST),
            "a delete"
        );
        for (sync, rule) in [
            (list_update(renamed), RULE_LIST_UNKNOWN),
            (delete(LIST), RULE_LIST_UNKNOWN),
            (d2d::DistributionListSync { action: None }, RULE_SYNC_EMPTY),
            (
                list_action(d2d::distribution_list_sync::Action::Create(
                    d2d::distribution_list_sync::Create {
                        distribution_list: None,
                    },
                )),
                RULE_SYNC_EMPTY,
            ),
        ] {
            let got = list_sync(&sync, &Rows::default()).expect("maps");
            assert!(
                got.dropped == Some(rule) && got.writes.is_empty() && got.events.is_empty(),
                "{rule}"
            );
        }
        // 10,000 lists: a new one is over the bound, a held one is still updated.
        let full = Rows {
            lists: vec![full_list()],
            list_count: MAX_LISTS,
            ..Rows::default()
        };
        let mut eleventh = new.clone();
        eleventh.distribution_list_id = LIST + 1;
        assert_eq!(
            list_sync(&create(eleventh.clone()), &full)
                .expect("maps")
                .dropped,
            Some(RULE_OVER_BOUND)
        );
        let below = Rows {
            list_count: MAX_LISTS - 1,
            ..Rows::default()
        };
        assert!(
            quiet(&list_sync(&create(eleventh), &below).expect("maps")),
            "N lists"
        );
        assert!(
            quiet(&list_sync(&create(new), &full).expect("maps")),
            "a held list at the bound"
        );
    }
}
