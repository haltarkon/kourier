//! What a joined account states to the core (TH4 J4b, brief §2.6 with decision H1's two zones): ONE
//! pure function, [`events`], over a [`Snapshot`] of what the import stored. The join's zone 2 writes its
//! rows into `core_outbox` (`store::ready_join`) and `Rebuild` writes them again over the stored file
//! (the actor's `state_ready_again`): the same function over the same rows, so a rebuilt core holds what
//! the first one held.
//!
//! The rows, in order:
//! 1. `[AccountReady { own_ids: [threema:<ID>], device: None, display_name: the profile's nickname,
//!    number: None }, AccountSettings { read receipts, typing indicators }]` (AccountReady FIRST: the
//!    contract's rule, `kourier-backend-api` `event.rs`; no `AccountSettings` when the file has no settings
//!    row, a TH1 file);
//! 2. the contacts (by Threema ID), then the 1:1 chats of the DIRECT contacts (by Threema ID; TH5 A7b,
//!    decision E1), then the groups (by creator, then group id), [`ROW_EVENTS`] events a row at most.
//!
//! The map: a contact -> `ContactUpsert` (user `threema:<ID>`, profile name = its nickname, system name =
//!    first + last name, hidden = acquaintance level `GROUP_OR_DELETED`, blocked = in the settings'
//!    blocked identities); a DIRECT contact also -> its 1:1 chat's `ConversationUpsert` ([`direct_patch`]);
//!    a group -> `ConversationUpsert` with [`group_key`], title = its name, members =
//!    the creator (`Admin`, L14), the member identities (`Member`) and the own ID (`Member`, while the user
//!    state is `MEMBER` and the own ID is not the creator: the spec's list holds neither the creator nor the
//!    user); a distribution list -> nothing yet (as Desktop, `join.ts` `TODO(DESK-236)`). Never a key, never
//!    a picture (the held blobs become pictures in a later step). Every chat carries Threema's [`features`]
//!    and the phone's protected flag, archive, pin and mute ([`chat_state`], TH5 A7b).
//!
//! Pure and deterministic: the same snapshot gives the same rows, whatever order the phone sent.

use std::collections::HashSet;
use std::fmt::Write as _;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    AccountSettingsPatch, AttachmentKind, ContactPatch, ConvKey, ConversationFeatures,
    ConversationPatch, FileLimits, Member, MemberList, MemberRole, Patch, Support, UserId, Window,
};
use libthreema::protobuf::d2d_sync;
use libthreema::protobuf::d2d_sync::contact::AcquaintanceLevel;
use libthreema::protobuf::d2d_sync::group::UserState;
use libthreema::protobuf::d2d_sync::{
    ConversationCategory, ConversationVisibility, ReadReceiptPolicy, TypingIndicatorPolicy,
};

use super::essential::Import;

/// The most events of one `core_outbox` row after the first (one bounded core transaction, as
/// `kourier_backend_api::MAX_HISTORY_BATCH`).
pub(crate) const ROW_EVENTS: usize = 500;

/// The scheme of a Threema user (`threema:<ID>`) and of a Threema group key.
pub(crate) const SCHEME: &str = "threema";

/// A group as stored: its key columns and its `d2d_sync.Group`.
#[derive(Clone, PartialEq)]
pub(crate) struct SnapGroup {
    pub(crate) creator: String,
    pub(crate) group_id: u64,
    pub(crate) sync: d2d_sync::Group,
}

/// What a joined file holds that the core is told: read back by `store::load_snapshot`, or made from an
/// [`Import`] in memory ([`Snapshot::of`]). `Debug` prints counts only.
#[derive(Clone, PartialEq)]
pub(crate) struct Snapshot {
    /// The own Threema ID (personal data).
    pub(crate) own_id: String,
    /// `None`: no `own_profile` row (a TH1 file).
    pub(crate) profile: Option<d2d_sync::UserProfile>,
    /// `None`: no `settings` row (a TH1 file).
    pub(crate) settings: Option<d2d_sync::Settings>,
    pub(crate) contacts: Vec<d2d_sync::Contact>,
    pub(crate) groups: Vec<SnapGroup>,
    /// Stored, stated to nobody yet.
    pub(crate) lists: Vec<d2d_sync::DistributionList>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("profile", &self.profile.is_some())
            .field("settings", &self.settings.is_some())
            .field("contacts", &self.contacts.len())
            .field("groups", &self.groups.len())
            .field("lists", &self.lists.len())
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    /// The snapshot an import WILL leave in the file (the oracle of the store's round trip).
    pub(crate) fn of(import: &Import) -> Self {
        Self {
            own_id: import.threema_id.clone(),
            profile: Some(import.profile.clone()),
            settings: Some(import.settings.clone()),
            contacts: import.contacts.iter().map(|c| c.sync.clone()).collect(),
            groups: import
                .groups
                .iter()
                .map(|g| SnapGroup {
                    creator: g.creator.clone(),
                    group_id: g.group_id,
                    sync: g.sync.clone(),
                })
                .collect(),
            lists: import.lists.iter().map(|l| l.sync.clone()).collect(),
        }
    }
}

/// An id of the snapshot is not a valid `UserId` (checked data never is; a damaged file can be).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BadId;

/// `threema:<ID>`.
pub(crate) fn user(id: &str) -> Result<UserId, BadId> {
    UserId::from_parts(SCHEME, id).map_err(|_| BadId)
}

/// THE KEY OF A THREEMA GROUP (TH4 decides it, TH5 and every later step keep it): `ConvKey::Group` of
/// `threema:<creator ID>.<group id>`, the group id as 16 lower-case hex digits of its 8 bytes in
/// little-endian order (the wire order of the `fixed64`). The text is Desktop's `groupDebugString`
/// (`common/model/group.ts`: `${creator}.${u64ToHexLe(groupId)}`).
pub(crate) fn group_key(creator: &str, group_id: u64) -> Result<ConvKey, BadId> {
    let mut text = format!("{SCHEME}:{creator}.");
    for byte in group_id.to_le_bytes() {
        let _ = write!(text, "{byte:02x}");
    }
    let key = ConvKey::Group(text.into());
    key.validate().map_err(|_| BadId)?;
    Ok(key)
}

/// The inverse of [`group_key`]: the creator's ID and the group id of a `threema:` group's key. `None` for any other
/// text, so one group has one key (upper-case digits and a key of another network are no group here).
pub(crate) fn parse_group_key(key: &str) -> Option<(String, u64)> {
    let (creator, digits) = key
        .strip_prefix(SCHEME)?
        .strip_prefix(':')?
        .rsplit_once('.')?;
    let digits = digits.as_bytes();
    if digits.len() != 16 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0_u8; 8];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
        *byte = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some((creator.to_owned(), u64::from_le_bytes(bytes)))
}

/// A present name: set, or CLEARED when it is empty (decision M1: "empty strings have a different
/// semantic than an omitted string", `md-d2d-sync.proto`; a cleared name on the phone is an empty
/// string). An absent one changes nothing. The same for the join, a sync delta and a Rebuild.
fn name(text: Option<&str>) -> Patch<String> {
    match text.map(str::trim) {
        Some("") => Patch::Clear,
        Some(t) => Patch::Set(t.to_owned()),
        None => Patch::Unchanged,
    }
}

/// The first and last name of a contact, joined by a space; both absent: unchanged; present and both
/// empty: cleared (M1).
fn full_name(contact: &d2d_sync::Contact) -> Patch<String> {
    if contact.first_name.is_none() && contact.last_name.is_none() {
        return Patch::Unchanged;
    }
    let parts: Vec<&str> = [contact.first_name.as_deref(), contact.last_name.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    name(Some(&parts.join(" ")))
}

/// The settings' two switches the core knows; an absent or unknown policy is not stated. The join, a
/// reflected `SettingsSync` (TH5b B4, R13: over the WHOLE new message) and a Rebuild state it alike.
pub(crate) fn settings_patch(settings: &d2d_sync::Settings) -> AccountSettingsPatch {
    let read_receipts = settings
        .read_receipt_policy
        .and_then(|p| ReadReceiptPolicy::try_from(p).ok())
        .map(|p| p == ReadReceiptPolicy::SendReadReceipt);
    let typing_indicators = settings
        .typing_indicator_policy
        .and_then(|p| TypingIndicatorPolicy::try_from(p).ok())
        .map(|p| p == TypingIndicatorPolicy::SendTypingIndicator);
    AccountSettingsPatch {
        read_receipts,
        typing_indicators,
        ..AccountSettingsPatch::default()
    }
}

/// A stored contact as the core's `ContactUpsert` (module doc): the join, a `ContactSync` (TH5 A7a) and a
/// Rebuild state it from the WHOLE stored message, so all three say the same.
pub(crate) fn contact_event(
    contact: &d2d_sync::Contact,
    blocked: &HashSet<&str>,
) -> Result<BackendEvent, BadId> {
    let mut patch = ContactPatch::new(user(&contact.identity)?);
    patch.profile_name = name(contact.nickname.as_deref());
    patch.system_name = full_name(contact);
    patch.hidden =
        Some(contact.acquaintance_level == Some(AcquaintanceLevel::GroupOrDeleted as i32));
    patch.blocked = Some(blocked.contains(contact.identity.as_str()));
    Ok(BackendEvent::ContactUpsert(patch))
}

/// THREEMA'S FEATURES, on every Threema chat (1:1 and group; decision H2 with his QD), replaced whole with
/// each `ConversationUpsert`:
/// - the RECEIVE windows OPEN (`Window { None, None }`): the spec applies an edit's and a delete's time
///   constraint "on the sender side only. The receiver will always accept" (`csp-e2e.proto`), and the core
///   drops an edit or delete outside a stated window;
/// - the SEND side (TH8) is a text or one file: no edit, no delete for everyone, no reply, no reactions offered; a delete
///   for me stays
///   local (`delete_for_me: false`: nothing reaches the network); no admin delete (Threema has none);
/// - `reactions_per_user: None`: Threema keeps a SET of reactions per user;
/// - `files` (TH5b B1, decision R3 F2): EVERY attachment kind with a limit and `caption: true`,
///   only so the core takes a received edit of a file's CAPTION (it checks that the chat's `files` say every
///   kind of the message takes a caption). TH8: a text is held under Desktop's limit (`MAX_TEXT_BYTES`: PREPARE's box has
///   random padding, so the last bytes under its own limit are a matter of chance), a file under what Threema carries.
///
/// Everything else is the contract's most restrictive default (no timers, no group actions).
pub(crate) fn features() -> ConversationFeatures {
    let open = Window {
        max_age_secs: None,
        max_count: None,
    };
    ConversationFeatures {
        edit_receive: Some(open),
        delete_receive: Some(open),
        edit: None,
        delete_for_everyone: None,
        reply: Support::Unsupported,
        reactions: Support::Unsupported,
        reactions_per_user: None,
        delete_for_me: false,
        admin_delete: false,
        max_text_bytes: Some(MAX_TEXT_BYTES),
        files: ATTACHMENT_KINDS
            .into_iter()
            .map(|kind| {
                (
                    kind,
                    FileLimits {
                        max_size: crate::blob::MAX_FILE as u64,
                        caption: true,
                    },
                )
            })
            .collect(),
        ..ConversationFeatures::default()
    }
}

/// The longest text a composer takes (Desktop's `MAX_TEXT_MESSAGE_BYTES`).
const MAX_TEXT_BYTES: u32 = 6_000;

/// Every [`AttachmentKind`] (the six of the model; the unit test holds the list against an exhaustive match,
/// so a kind the model adds is a failing test here).
const ATTACHMENT_KINDS: [AttachmentKind; 6] = [
    AttachmentKind::Image,
    AttachmentKind::Video,
    AttachmentKind::Audio,
    AttachmentKind::Voice,
    AttachmentKind::Gif,
    AttachmentKind::File,
];

/// A chat's mute as the phone's `notification_trigger_policy_override` says it (his QC, decision M8), the
/// contact's and the group's oneof read into one shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mute {
    /// `default` (the settings' policy: every message), or a group's `MENTIONED` (M8: "only when
    /// mentioned" counts as NOT muted, so no mention is missed): not muted.
    Off,
    /// `NEVER`, until `expires_at` (ms) or, without it, for ever.
    Never(Option<u64>),
    /// Absent, an unset oneof or a policy the pinned proto does not know: nothing is stated.
    Unknown,
}

/// The muted-until value of a mute (`muted_until`, ms): `NEVER` without an end is for ever (`i64::MAX`, the
/// contract's "for ever" as Signal states it), with one it is that time (saturating), and not muted clears.
fn muted_until(mute: Mute) -> Patch<i64> {
    match mute {
        Mute::Off => Patch::Clear,
        Mute::Never(None) => Patch::Set(i64::MAX),
        Mute::Never(Some(at)) => Patch::Set(i64::try_from(at).unwrap_or(i64::MAX)),
        Mute::Unknown => Patch::Unchanged,
    }
}

fn contact_mute(contact: &d2d_sync::Contact) -> Mute {
    use d2d_sync::contact::notification_trigger_policy_override::{
        Override, policy::NotificationTriggerPolicy,
    };
    match contact
        .notification_trigger_policy_override
        .as_ref()
        .and_then(|o| o.r#override.as_ref())
    {
        Some(Override::Default(_)) => Mute::Off,
        Some(Override::Policy(p)) => match NotificationTriggerPolicy::try_from(p.policy) {
            Ok(NotificationTriggerPolicy::Never) => Mute::Never(p.expires_at),
            Err(_) => Mute::Unknown,
        },
        None => Mute::Unknown,
    }
}

fn group_mute(group: &d2d_sync::Group) -> Mute {
    use d2d_sync::group::notification_trigger_policy_override::{
        Override, policy::NotificationTriggerPolicy,
    };
    match group
        .notification_trigger_policy_override
        .as_ref()
        .and_then(|o| o.r#override.as_ref())
    {
        Some(Override::Default(_)) => Mute::Off,
        Some(Override::Policy(p)) => match NotificationTriggerPolicy::try_from(p.policy) {
            // M8: only when mentioned = not muted.
            Ok(NotificationTriggerPolicy::Mentioned) => Mute::Off,
            Ok(NotificationTriggerPolicy::Never) => Mute::Never(p.expires_at),
            Err(_) => Mute::Unknown,
        },
        None => Mute::Unknown,
    }
}

/// THE CHAT'S STATE FROM THE PHONE (decisions E2, QC), the same for a 1:1 chat and a group, set on `patch`:
/// - `features` ([`features`]);
/// - `protected` (E2, his QA): `conversation_category` PROTECTED -> `Some(true)`, DEFAULT -> `Some(false)`
///   (the phone changed it back), absent or unknown -> `None`;
/// - archive and pin (QC, `conversation_visibility`): NORMAL -> not archived, not pinned; PINNED -> not
///   archived, pinned at `0` (every pinned Threema chat at the same place: the core orders them by their
///   last activity, as the phone does; Threema keeps no order among pinned chats); ARCHIVED -> archived,
///   not pinned; absent or unknown -> unchanged;
/// - mute (QC, M8): [`muted_until`] of the override.
///
/// The phone's `last_update_at` is NOT stated: the contract carries no such field (the core's
/// `last_activity` is its own, from the messages).
fn chat_state(
    patch: &mut ConversationPatch,
    category: Option<i32>,
    visibility: Option<i32>,
    mute: Mute,
) {
    patch.features = Some(features());
    patch.protected = category
        .and_then(|c| ConversationCategory::try_from(c).ok())
        .map(|c| c == ConversationCategory::Protected);
    match visibility.and_then(|v| ConversationVisibility::try_from(v).ok()) {
        Some(ConversationVisibility::Normal) => {
            patch.archived = Some(false);
            patch.pinned_order = Patch::Clear;
        }
        Some(ConversationVisibility::Pinned) => {
            patch.archived = Some(false);
            patch.pinned_order = Patch::Set(0);
        }
        Some(ConversationVisibility::Archived) => {
            patch.archived = Some(true);
            patch.pinned_order = Patch::Clear;
        }
        None => {}
    }
    patch.muted_until = muted_until(mute);
}

/// Whether the phone lists `contact` as a DIRECT contact: every acquaintance level but `GROUP_OR_DELETED`
/// (the one `contact_event` states as hidden).
pub(crate) fn is_direct(contact: &d2d_sync::Contact) -> bool {
    contact.acquaintance_level != Some(AcquaintanceLevel::GroupOrDeleted as i32)
}

/// THE SEAM of a stored contact's 1:1 CHAT (TH5 A7b; decision E1, his QB "All at once, empty"): the
/// `ConversationUpsert` patch of `Direct(threema:<ID>)` with [`chat_state`] from the WHOLE stored message, or
/// `None` for a contact the phone HIDES (acquaintance level `GROUP_OR_DELETED`): the lead's call, nothing is
/// stated for its chat (never `ConversationRemoved`, which deletes the history; the contract has no
/// conversation-level hide, and archiving it would state an archive the phone never made). The join's zone
/// 2, a Rebuild and a contact sync (`reflect::sync::contact_sync`) state it from here.
pub(crate) fn direct_patch(
    contact: &d2d_sync::Contact,
) -> Result<Option<ConversationPatch>, BadId> {
    if !is_direct(contact) {
        return Ok(None);
    }
    one_to_one_patch(contact).map(Some)
}

/// The 1:1 chat's patch of a stored contact WHATEVER its acquaintance level: the key and [`chat_state`]
/// (features, protected, archive / pin, mute). [`direct_patch`] for a direct contact; for a
/// HIDDEN one only `reflect::map` asks (TH5 A8, E5: its message makes the core create the chat, which then
/// carries the features first in the same row).
pub(crate) fn one_to_one_patch(contact: &d2d_sync::Contact) -> Result<ConversationPatch, BadId> {
    let mut patch = ConversationPatch::new(ConvKey::Direct(user(&contact.identity)?));
    chat_state(
        &mut patch,
        contact.conversation_category,
        contact.conversation_visibility,
        contact_mute(contact),
    );
    Ok(patch)
}

/// A stored contact's 1:1 chat as the core's `ConversationUpsert` ([`direct_patch`]).
pub(crate) fn direct_event(contact: &d2d_sync::Contact) -> Result<Option<BackendEvent>, BadId> {
    Ok(direct_patch(contact)?.map(BackendEvent::ConversationUpsert))
}

/// Whether a stored group is shown at all (decision M1, `md-d2d-sync.proto` `member_identities`): a group
/// the user was KICKED from or LEFT whose list of previous members is EMPTY "must not be visible in the UI".
pub(crate) fn is_visible(group: &d2d_sync::Group) -> bool {
    is_member(group)
        || group
            .member_identities
            .as_ref()
            .is_some_and(|m| !m.identities.is_empty())
}

/// Whether the user is a member of the stored group now: not `KICKED` and not `LEFT` (an absent state is the proto's
/// default, `MEMBER`).
pub(crate) fn is_member(group: &d2d_sync::Group) -> bool {
    group
        .user_state
        .is_none_or(|s| s == UserState::Member as i32)
}

/// THE SEAM of a stored group (TH5 A7a, A7b): its conversation patch, or `None` when it is not visible
/// ([`is_visible`]). Key [`group_key`], title = its name (M1: an empty one clears), members = the creator
/// first (`Admin`, L14), then the member identities and the own ID (`Member`; the own ID while the user
/// state is `MEMBER` and the user is not the creator: the spec's list holds neither), these SORTED by ID,
/// each once; then the features, the protected flag, archive / pin and mute ([`chat_state`]).
pub(crate) fn group_patch(
    group: &SnapGroup,
    own: &str,
) -> Result<Option<ConversationPatch>, BadId> {
    if !is_visible(&group.sync) {
        return Ok(None);
    }
    let mut patch = ConversationPatch::new(group_key(&group.creator, group.group_id)?);
    patch.title = name(group.sync.name.as_deref());
    let listed = group
        .sync
        .member_identities
        .as_ref()
        .map_or(&[][..], |m| m.identities.as_slice());
    let member_now = group
        .sync
        .user_state
        .is_none_or(|s| s == UserState::Member as i32);
    let mut ids: Vec<&str> = listed
        .iter()
        .map(String::as_str)
        .filter(|id| *id != group.creator && *id != own)
        .collect();
    if member_now && group.creator != own {
        ids.push(own);
    }
    ids.sort_unstable();
    ids.dedup();
    let mut members = vec![Member {
        user: user(&group.creator)?,
        role: MemberRole::Admin,
    }];
    for id in ids {
        members.push(Member {
            user: user(id)?,
            role: MemberRole::Member,
        });
    }
    patch.members = Some(MemberList::snapshot(members));
    chat_state(
        &mut patch,
        group.sync.conversation_category,
        group.sync.conversation_visibility,
        group_mute(&group.sync),
    );
    Ok(Some(patch))
}

/// A stored group as the core's `ConversationUpsert`, or nothing when it is not visible.
pub(crate) fn group_event(group: &SnapGroup, own: &str) -> Result<Option<BackendEvent>, BadId> {
    Ok(group_patch(group, own)?.map(BackendEvent::ConversationUpsert))
}

/// The `core_outbox` rows of `snapshot` (module doc): row 1 starts with `AccountReady`, every later row
/// holds at most [`ROW_EVENTS`] events. Fails only on an id that is no valid `UserId`.
pub(crate) fn events(snapshot: &Snapshot) -> Result<Vec<Vec<BackendEvent>>, BadId> {
    let nickname = snapshot
        .profile
        .as_ref()
        .and_then(|p| p.nickname.as_deref());
    let mut first =
        vec![crate::store::account_ready(&snapshot.own_id, nickname).map_err(|_| BadId)?];
    if let Some(settings) = &snapshot.settings {
        first.push(BackendEvent::AccountSettings(settings_patch(settings)));
    }

    let blocked: HashSet<&str> = snapshot
        .settings
        .as_ref()
        .and_then(|s| s.blocked_identities.as_ref())
        .map(|b| b.identities.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let mut contacts: Vec<&d2d_sync::Contact> = snapshot.contacts.iter().collect();
    contacts.sort_by(|a, b| a.identity.cmp(&b.identity));
    let mut groups: Vec<&SnapGroup> = snapshot.groups.iter().collect();
    groups.sort_by(|a, b| (&a.creator, a.group_id).cmp(&(&b.creator, b.group_id)));

    let mut rest = Vec::with_capacity(2 * contacts.len() + groups.len());
    for contact in &contacts {
        rest.push(contact_event(contact, &blocked)?);
    }
    // E1: the 1:1 chat of every direct contact, after the contacts (so its person is known), before the
    // groups.
    for contact in contacts {
        rest.extend(direct_event(contact)?);
    }
    for group in groups {
        rest.extend(group_event(group, &snapshot.own_id)?);
    }
    // A distribution list: nothing yet (Desktop imports none either, `TODO(DESK-236)`).

    let mut rows = Vec::with_capacity(1 + rest.len().div_ceil(ROW_EVENTS));
    rows.push(first);
    let mut rest = rest.into_iter().peekable();
    while rest.peek().is_some() {
        rows.push(rest.by_ref().take(ROW_EVENTS).collect());
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use libthreema::protobuf::common;

    use super::*;

    /// Synthetic IDs (8 characters), each a substring of no other test value.
    const OWN: &str = "J4BOWN01";
    const C0: &str = "J4BC0000";
    const C1: &str = "J4BC0001";
    const C2: &str = "J4BC0002";

    fn contact(id: &str) -> d2d_sync::Contact {
        d2d_sync::Contact {
            identity: id.to_owned(),
            public_key: Some(vec![0x51; 32]),
            ..Default::default()
        }
    }

    fn group(creator: &str, group_id: u64, members: &[&str]) -> SnapGroup {
        SnapGroup {
            creator: creator.to_owned(),
            group_id,
            sync: d2d_sync::Group {
                group_identity: Some(common::GroupIdentity {
                    group_id,
                    creator_identity: creator.to_owned(),
                }),
                name: Some("Group J4B".to_owned()),
                user_state: Some(UserState::Member as i32),
                member_identities: Some(common::Identities {
                    identities: members.iter().map(|m| (*m).to_owned()).collect(),
                }),
                ..Default::default()
            },
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            own_id: OWN.to_owned(),
            profile: Some(d2d_sync::UserProfile {
                nickname: Some("Own J4B".to_owned()),
                ..Default::default()
            }),
            settings: Some(d2d_sync::Settings {
                read_receipt_policy: Some(ReadReceiptPolicy::DontSendReadReceipt as i32),
                typing_indicator_policy: Some(TypingIndicatorPolicy::SendTypingIndicator as i32),
                blocked_identities: Some(common::Identities {
                    identities: vec![C1.to_owned()],
                }),
                ..Default::default()
            }),
            contacts: vec![
                d2d_sync::Contact {
                    nickname: Some("Nick J4B".to_owned()),
                    first_name: Some("First J4B".to_owned()),
                    last_name: Some("Last J4B".to_owned()),
                    acquaintance_level: Some(AcquaintanceLevel::Direct as i32),
                    ..contact(C0)
                },
                contact(C1),
                d2d_sync::Contact {
                    acquaintance_level: Some(AcquaintanceLevel::GroupOrDeleted as i32),
                    ..contact(C2)
                },
            ],
            groups: vec![
                group(C0, 0x0102_0304_0506_0708, &[C1, C2]),
                group(OWN, 7, &[C0]),
            ],
            lists: vec![d2d_sync::DistributionList {
                distribution_list_id: 9,
                ..Default::default()
            }],
        }
    }

    fn uid(id: &str) -> UserId {
        UserId::from_parts("threema", id).expect("a valid id")
    }

    fn contact_of<'a>(rows: &'a [Vec<BackendEvent>], id: &str) -> &'a ContactPatch {
        let user = uid(id);
        rows.iter()
            .flatten()
            .find_map(|e| match e {
                BackendEvent::ContactUpsert(p) if p.user == user => Some(p),
                _ => None,
            })
            .expect("the contact is stated")
    }

    fn conversations(rows: &[Vec<BackendEvent>]) -> Vec<&ConversationPatch> {
        rows.iter()
            .flatten()
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    /// The groups' conversations (the 1:1 chats left out).
    fn group_convs(rows: &[Vec<BackendEvent>]) -> Vec<&ConversationPatch> {
        conversations(rows)
            .into_iter()
            .filter(|p| matches!(p.key, ConvKey::Group(_)))
            .collect()
    }

    fn direct(id: &str) -> ConvKey {
        ConvKey::Direct(uid(id))
    }

    /// H2 written out with EVERY field named (no `..`): the receive windows open, the send side off,
    /// a chat that can be sent in, a set of reactions per user, every attachment kind with its limit and a caption
    /// (TH5b R3 F2), the rest the contract's most restrictive values.
    fn h2_features() -> ConversationFeatures {
        let open = Window {
            max_age_secs: None,
            max_count: None,
        };
        ConversationFeatures {
            max_text_bytes: Some(MAX_TEXT_BYTES),
            formatting: std::collections::BTreeMap::new(),
            files: [
                AttachmentKind::Image,
                AttachmentKind::Video,
                AttachmentKind::Audio,
                AttachmentKind::Voice,
                AttachmentKind::Gif,
                AttachmentKind::File,
            ]
            .into_iter()
            .map(|kind| {
                (
                    kind,
                    FileLimits {
                        max_size: crate::blob::MAX_FILE as u64,
                        caption: true,
                    },
                )
            })
            .collect(),
            file_packing: kourier_model::FilePacking::default(),
            reply: Support::Unsupported,
            edit: None,
            delete_for_everyone: None,
            delete_for_me: false,
            edit_receive: Some(open),
            delete_receive: Some(open),
            admin_delete: false,
            reactions: Support::Unsupported,
            reactions_per_user: None,
            typing: false,
            read_receipts: false,
            delivery_receipts: false,
            disappearing: std::collections::BTreeMap::new(),
            disappearing_custom: std::collections::BTreeMap::new(),
            hard_to_save: std::collections::BTreeSet::new(),
            history: kourier_model::HistorySource::None,
            group_actions: kourier_model::GroupActions::default(),
            group_info: None,
            cannot_send: None,
            roaming_listed: None,
            invite_link_on: false,
        }
    }

    fn contact_policy(policy: i32, expires_at: Option<u64>) -> d2d_sync::Contact {
        use d2d_sync::contact::notification_trigger_policy_override as o;
        d2d_sync::Contact {
            notification_trigger_policy_override: Some(
                d2d_sync::contact::NotificationTriggerPolicyOverride {
                    r#override: Some(o::Override::Policy(o::Policy { policy, expires_at })),
                },
            ),
            ..contact(C0)
        }
    }

    fn group_policy(policy: i32, expires_at: Option<u64>) -> SnapGroup {
        use d2d_sync::group::notification_trigger_policy_override as o;
        let mut g = group(C0, 1, &[C1]);
        g.sync.notification_trigger_policy_override =
            Some(d2d_sync::group::NotificationTriggerPolicyOverride {
                r#override: Some(o::Override::Policy(o::Policy { policy, expires_at })),
            });
        g
    }

    fn direct_of(c: &d2d_sync::Contact) -> ConversationPatch {
        direct_patch(c)
            .expect("a valid id")
            .expect("a direct contact")
    }

    fn group_of(g: &SnapGroup) -> ConversationPatch {
        group_patch(g, OWN)
            .expect("a valid id")
            .expect("a visible group")
    }

    #[test]
    fn row_one_is_account_ready_with_the_nickname_then_the_settings() {
        let rows = events(&snapshot()).expect("events");
        match rows[0].as_slice() {
            [
                BackendEvent::AccountReady {
                    own_ids,
                    device,
                    display_name,
                    number,
                },
                BackendEvent::AccountSettings(settings),
            ] => {
                assert!(own_ids.as_slice() == [uid(OWN)], "the own id");
                assert_eq!(*device, None);
                assert!(
                    display_name.as_ref().map(|n| n.expose().as_str()) == Some("Own J4B"),
                    "AccountReady does not carry the profile's nickname"
                );
                assert!(number.is_none());
                assert_eq!(settings.read_receipts, Some(false), "read receipts off");
                assert_eq!(settings.typing_indicators, Some(true), "typing on");
                assert_eq!(
                    *settings,
                    AccountSettingsPatch {
                        read_receipts: Some(false),
                        typing_indicators: Some(true),
                        ..AccountSettingsPatch::default()
                    },
                    "only the two switches are stated"
                );
            }
            _ => panic!("row 1 is not [AccountReady, AccountSettings]"),
        }
        // No nickname, no settings row (a TH1 file): AccountReady alone, no name.
        let mut bare = snapshot();
        bare.profile = None;
        bare.settings = None;
        let rows = events(&bare).expect("events");
        assert!(
            matches!(
                rows[0].as_slice(),
                [BackendEvent::AccountReady {
                    display_name: None,
                    ..
                }]
            ),
            "a file without profile and settings states AccountReady alone"
        );
    }

    #[test]
    fn a_contact_is_stated_with_its_names_and_flags() {
        let rows = events(&snapshot()).expect("events");
        let c0 = contact_of(&rows, C0);
        assert!(
            c0.profile_name == Patch::Set("Nick J4B".to_owned()),
            "the profile name is the contact's nickname"
        );
        assert!(
            c0.system_name == Patch::Set("First J4B Last J4B".to_owned()),
            "the system name is first + last"
        );
        assert!(
            c0.nickname == Patch::Unchanged,
            "our own nickname is not set"
        );
        assert_eq!((c0.hidden, c0.blocked), (Some(false), Some(false)));
        assert!(c0.avatar == Patch::Unchanged, "never a picture");
        assert!(c0.identity_fingerprint == Patch::Unchanged, "never a key");
        let c1 = contact_of(&rows, C1);
        assert_eq!(
            (c1.hidden, c1.blocked),
            (Some(false), Some(true)),
            "blocked"
        );
        assert!(
            c1.profile_name == Patch::Unchanged && c1.system_name == Patch::Unchanged,
            "no names: unchanged"
        );
        let c2 = contact_of(&rows, C2);
        assert_eq!((c2.hidden, c2.blocked), (Some(true), Some(false)), "hidden");
        // Only a last name: no leading space.
        let mut one = snapshot();
        one.contacts[1].last_name = Some("Only J4B".to_owned());
        let rows = events(&one).expect("events");
        assert!(contact_of(&rows, C1).system_name == Patch::Set("Only J4B".to_owned()));
    }

    #[test]
    fn a_group_is_a_conversation_with_its_key_title_members_and_the_creator_as_admin() {
        let rows = events(&snapshot()).expect("events");
        let groups = group_convs(&rows);
        assert_eq!(groups.len(), 2, "one conversation per group, none per list");
        // Sorted by creator: "J4BC0000" before "J4BOWN01".
        let theirs = groups[0];
        assert_eq!(
            theirs.key.encode(),
            "group:threema:J4BC0000.0807060504030201",
            "the Threema group key (TH5 keeps it)"
        );
        assert!(
            theirs.title == Patch::Set("Group J4B".to_owned()),
            "the title"
        );
        let member = |id: &str, role| Member {
            user: uid(id),
            role,
        };
        assert_eq!(
            theirs.members,
            Some(MemberList::snapshot(vec![
                member(C0, MemberRole::Admin),
                member(C1, MemberRole::Member),
                member(C2, MemberRole::Member),
                member(OWN, MemberRole::Member),
            ])),
            "the creator as admin, the members, then the user"
        );
        assert!(theirs.avatar == Patch::Unchanged, "never a picture");
        let ours = groups[1];
        assert_eq!(ours.key.encode(), "group:threema:J4BOWN01.0700000000000000");
        assert_eq!(
            ours.members,
            Some(MemberList::snapshot(vec![
                member(OWN, MemberRole::Admin),
                member(C0, MemberRole::Member),
            ])),
            "the own group: the user is the admin, once"
        );
        // A group the user left: the previous members, not the user.
        let mut left = snapshot();
        left.groups[0].sync.user_state = Some(UserState::Left as i32);
        let rows = events(&left).expect("events");
        assert_eq!(
            group_convs(&rows)[0]
                .members
                .as_ref()
                .map(|m| m.upsert.len()),
            Some(3),
            "a left group does not list the user"
        );
        // The key's edges: 0 and u64::MAX, a Gateway creator.
        assert_eq!(
            group_key("*J4BGATE", 0).expect("key").encode(),
            "group:threema:*J4BGATE.0000000000000000"
        );
        assert_eq!(
            group_key(C0, u64::MAX).expect("key").encode(),
            "group:threema:J4BC0000.ffffffffffffffff"
        );
    }

    #[test]
    fn no_list_is_stated() {
        let rows = events(&snapshot()).expect("events");
        let all: Vec<&BackendEvent> = rows.iter().flatten().collect();
        // AccountReady, AccountSettings, 3 contacts, the 1:1 chats of the 2 direct ones (A7b, E1), 2 groups:
        // the list adds nothing.
        assert_eq!(all.len(), 9);
        let mut more = snapshot();
        more.lists.push(d2d_sync::DistributionList {
            distribution_list_id: 10,
            ..Default::default()
        });
        assert!(
            events(&more).expect("events") == rows,
            "a list changed the events"
        );
    }

    #[test]
    fn rows_after_the_first_hold_at_most_500_events() {
        let mut big = snapshot();
        big.groups.clear();
        big.contacts = (0..1_001).map(|i| contact(&format!("J4B{i:05}"))).collect();
        let rows = events(&big).expect("events");
        let sizes: Vec<usize> = rows.iter().map(Vec::len).collect();
        // 1,001 contacts and their 1,001 1:1 chats (A7b, E1).
        assert_eq!(sizes, vec![2, 500, 500, 500, 500, 2], "row sizes");
        let later: Vec<&BackendEvent> = rows[1..].iter().flatten().collect();
        assert!(
            later[..1_001]
                .iter()
                .all(|e| matches!(e, BackendEvent::ContactUpsert(_))),
            "the later rows start with the contacts"
        );
        assert!(
            later[1_001..].iter().all(|e| matches!(
                e,
                BackendEvent::ConversationUpsert(p) if matches!(p.key, ConvKey::Direct(_))
            )),
            "the contacts are followed by their 1:1 chats"
        );
        assert!(
            rows[1..]
                .iter()
                .flatten()
                .all(|e| !matches!(e, BackendEvent::AccountReady { .. })),
            "AccountReady only in row 1"
        );
    }

    #[test]
    fn the_events_are_deterministic_whatever_the_order_of_the_phone() {
        let one = events(&snapshot()).expect("events");
        let mut shuffled = snapshot();
        shuffled.contacts.reverse();
        shuffled.groups.reverse();
        assert!(
            events(&shuffled).expect("events") == one,
            "the order changed the events"
        );
        assert!(
            events(&snapshot()).expect("events") == one,
            "two runs differ"
        );
        // Contacts, then the direct contacts' 1:1 chats, then the groups, each sorted.
        let kinds: Vec<&str> = one[1].iter().map(BackendEvent::name).collect();
        assert_eq!(
            kinds,
            vec![
                "ContactUpsert",
                "ContactUpsert",
                "ContactUpsert",
                "ConversationUpsert",
                "ConversationUpsert",
                "ConversationUpsert",
                "ConversationUpsert"
            ]
        );
        let keys: Vec<ConvKey> = conversations(&one).iter().map(|p| p.key.clone()).collect();
        assert_eq!(keys[..2], [direct(C0), direct(C1)], "the 1:1 chats by ID");
        let users: Vec<UserId> = one[1]
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ContactUpsert(p) => Some(p.user.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec![uid(C0), uid(C1), uid(C2)]);
    }

    #[test]
    fn a_bad_id_fails_and_nothing_prints_a_name() {
        let mut bad = snapshot();
        bad.contacts[0].identity = "J4B C000".to_owned();
        assert_eq!(events(&bad), Err(BadId));
        let shown = format!(
            "{:?} {:?}",
            snapshot(),
            events(&snapshot()).expect("events")
        );
        for secret in [OWN, C0, "Own J4B", "Nick J4B", "First J4B", "Group J4B"] {
            assert!(
                !shown.contains(secret),
                "a Debug output shows a name or an ID"
            );
        }
    }

    /// The inverse of the key, literal both ways; a key that is not `group_key`'s text is no group: another network, no dot, a short
    /// or long id, upper-case digits, a character that is no digit.
    #[test]
    fn a_group_key_reads_back_and_no_other_text_does() {
        let key = "threema:KOURIER3.0807060504030201";
        assert_eq!(
            parse_group_key(key),
            Some(("KOURIER3".to_owned(), 0x0102_0304_0506_0708))
        );
        let ConvKey::Group(made) = group_key("KOURIER3", 0x0102_0304_0506_0708).expect("a key")
        else {
            panic!("a group's key is a group");
        };
        assert_eq!(&*made, key, "the key's own form");
        assert_eq!(
            parse_group_key("threema:*GATEWAY.ffffffffffffffff"),
            Some(("*GATEWAY".to_owned(), u64::MAX))
        );
        for text in [
            "",
            "threema:",
            "signal:KOURIER3.0807060504030201",
            "threema:KOURIER30807060504030201",
            "threema:KOURIER3.08070605040302",
            "threema:KOURIER3.080706050403020100",
            "threema:KOURIER3.0807060504030A01",
            "threema:KOURIER3.08070605040302g1",
            "threema:KOURIER3.+807060504030201",
        ] {
            assert_eq!(
                parse_group_key(text),
                None,
                "the text of case {:?}",
                text.len()
            );
        }
    }

    #[test]
    fn threema_features_are_h2s_receive_open_and_th8s_text_and_files_to_send() {
        assert_eq!(
            features(),
            h2_features(),
            "Threema's features differ from H2's"
        );
        assert_eq!(features().cannot_send, None);
        let f = features();
        for window in [f.edit_receive, f.delete_receive] {
            assert!(
                window.is_some_and(|w| w.allows(u64::MAX, u16::MAX)),
                "a receive window refuses an old edit or delete"
            );
        }
        // Every chat the join states carries them, 1:1 and group.
        let rows = events(&snapshot()).expect("events");
        let chats = conversations(&rows);
        assert_eq!(chats.len(), 4);
        assert!(
            chats.iter().all(|p| p.features == Some(h2_features())),
            "a stated chat lacks Threema's features"
        );
    }

    /// TH5b B2 (R6, R7): an edit or a delete is never refused for its age. The receive windows are stated OPEN
    /// on every chat (the spec applies the time constraint on the sender side only), and the core drops a
    /// received edit or delete only outside a stated window, so the oldest stand-in and a thousandth edit pass;
    /// the same check REFUSES a window that is not open, so this is no vacuous test.
    #[test]
    fn the_receive_windows_never_drop_an_edit_or_a_delete_by_age() {
        let open = Window {
            max_age_secs: None,
            max_count: None,
        };
        let f = features();
        assert_eq!(f.edit_receive, Some(open), "the edit window is not open");
        assert_eq!(
            f.delete_receive,
            Some(open),
            "the delete window is not open"
        );
        for window in [open, open] {
            assert!(window.allows(u64::MAX, u16::MAX));
            assert!(window.allows(0, 0));
        }
        let six_hours = Window {
            max_age_secs: Some(6 * 3600),
            max_count: None,
        };
        assert!(
            !six_hours.allows(6 * 3600 + 1, 0),
            "the check no longer refuses a window that is not open"
        );
        // Both group and 1:1 chats the build states say so (a left group included, `h2_features`).
        let rows = events(&snapshot()).expect("events");
        assert!(
            conversations(&rows).iter().all(|p| {
                p.features
                    .as_ref()
                    .is_some_and(|f| f.edit_receive == Some(open) && f.delete_receive == Some(open))
            }),
            "a stated chat has a window that is not open"
        );
    }

    /// TH5b B1 (R3, F2): the chat's `files` name EVERY attachment kind, each with a caption and the mirror's limit,
    /// so the core takes a received edit of a file's caption; nothing else about files is stated (no album
    /// packing: one file per message).
    #[test]
    fn the_files_state_every_attachment_kind_with_a_caption_and_the_mirrors_limit() {
        // An exhaustive match: a kind the model adds does not compile here until it is decided.
        let _every_kind = |kind: AttachmentKind| match kind {
            AttachmentKind::Image
            | AttachmentKind::Video
            | AttachmentKind::Audio
            | AttachmentKind::Voice
            | AttachmentKind::Gif
            | AttachmentKind::File => (),
        };
        let files = features().files;
        assert_eq!(files.len(), 6, "not six attachment kinds");
        for kind in [
            AttachmentKind::Image,
            AttachmentKind::Video,
            AttachmentKind::Audio,
            AttachmentKind::Voice,
            AttachmentKind::Gif,
            AttachmentKind::File,
        ] {
            assert_eq!(
                files.get(&kind),
                Some(&FileLimits {
                    max_size: crate::blob::MAX_FILE as u64,
                    caption: true
                }),
                "a kind lacks its limits: {kind:?}"
            );
        }
        assert_eq!(
            features().file_packing,
            kourier_model::FilePacking::default(),
            "files are packed although nothing is sent"
        );
        // Every chat the join states says it, 1:1 and group.
        let rows = events(&snapshot()).expect("events");
        assert!(
            conversations(&rows)
                .iter()
                .all(|p| p.features.as_ref().is_some_and(|f| f.files == files)),
            "a stated chat lacks the files"
        );
    }

    #[test]
    fn protected_follows_the_conversation_category_both_ways() {
        let cases = [
            (Some(ConversationCategory::Protected as i32), Some(true)),
            (Some(ConversationCategory::Default as i32), Some(false)),
            (None, None),
            (Some(7), None),
        ];
        for (category, want) in cases {
            let c = d2d_sync::Contact {
                conversation_category: category,
                ..contact(C0)
            };
            assert_eq!(direct_of(&c).protected, want, "1:1 {category:?}");
            let mut g = group(C0, 1, &[C1]);
            g.sync.conversation_category = category;
            assert_eq!(group_of(&g).protected, want, "group {category:?}");
        }
    }

    #[test]
    fn the_visibility_is_normal_pinned_or_archived() {
        let cases = [
            (
                Some(ConversationVisibility::Normal as i32),
                Some(false),
                Patch::Clear,
            ),
            (
                Some(ConversationVisibility::Pinned as i32),
                Some(false),
                Patch::Set(0),
            ),
            (
                Some(ConversationVisibility::Archived as i32),
                Some(true),
                Patch::Clear,
            ),
            (None, None, Patch::Unchanged),
            (Some(9), None, Patch::Unchanged),
        ];
        for (visibility, archived, pinned) in cases {
            let c = d2d_sync::Contact {
                conversation_visibility: visibility,
                ..contact(C0)
            };
            let p = direct_of(&c);
            assert_eq!(
                (p.archived, p.pinned_order),
                (archived, pinned),
                "1:1 {visibility:?}"
            );
            let mut g = group(C0, 1, &[C1]);
            g.sync.conversation_visibility = visibility;
            let p = group_of(&g);
            assert_eq!(
                (p.archived, p.pinned_order),
                (archived, pinned),
                "group {visibility:?}"
            );
        }
    }

    #[test]
    fn a_mute_with_and_without_expires_at_and_mentioned_is_not_muted() {
        use d2d_sync::contact::notification_trigger_policy_override::policy::NotificationTriggerPolicy as CPolicy;
        use d2d_sync::group::notification_trigger_policy_override::policy::NotificationTriggerPolicy as GPolicy;
        const AT: u64 = 1_761_400_000_000;
        let at = i64::try_from(AT).expect("fits");
        // A contact: NEVER for ever, NEVER until AT, past i64, an unknown policy.
        let never = CPolicy::Never as i32;
        assert_eq!(
            direct_of(&contact_policy(never, None)).muted_until,
            Patch::Set(i64::MAX)
        );
        assert_eq!(
            direct_of(&contact_policy(never, Some(AT))).muted_until,
            Patch::Set(at)
        );
        assert_eq!(
            direct_of(&contact_policy(never, Some(u64::MAX))).muted_until,
            Patch::Set(i64::MAX)
        );
        assert_eq!(
            direct_of(&contact_policy(5, None)).muted_until,
            Patch::Unchanged
        );
        // The default policy: not muted (cleared); no override, or an unset oneof: unchanged.
        let mut c = contact(C0);
        c.notification_trigger_policy_override =
            Some(d2d_sync::contact::NotificationTriggerPolicyOverride {
                r#override: Some(
                    d2d_sync::contact::notification_trigger_policy_override::Override::Default(
                        common::Unit {},
                    ),
                ),
            });
        assert_eq!(direct_of(&c).muted_until, Patch::Clear);
        assert_eq!(direct_of(&contact(C0)).muted_until, Patch::Unchanged);
        c.notification_trigger_policy_override =
            Some(d2d_sync::contact::NotificationTriggerPolicyOverride { r#override: None });
        assert_eq!(direct_of(&c).muted_until, Patch::Unchanged);
        // A group: M8, MENTIONED (with and without an end) is NOT muted; NEVER is.
        let mentioned = GPolicy::Mentioned as i32;
        assert_eq!(
            group_of(&group_policy(mentioned, None)).muted_until,
            Patch::Clear
        );
        assert_eq!(
            group_of(&group_policy(mentioned, Some(AT))).muted_until,
            Patch::Clear
        );
        let never = GPolicy::Never as i32;
        assert_eq!(
            group_of(&group_policy(never, None)).muted_until,
            Patch::Set(i64::MAX)
        );
        assert_eq!(
            group_of(&group_policy(never, Some(AT))).muted_until,
            Patch::Set(at)
        );
        assert_eq!(
            group_of(&group_policy(5, None)).muted_until,
            Patch::Unchanged
        );
        let mut g = group(C0, 1, &[C1]);
        g.sync.notification_trigger_policy_override =
            Some(d2d_sync::group::NotificationTriggerPolicyOverride {
                r#override: Some(
                    d2d_sync::group::notification_trigger_policy_override::Override::Default(
                        common::Unit {},
                    ),
                ),
            });
        assert_eq!(group_of(&g).muted_until, Patch::Clear);
        assert_eq!(group_of(&group(C0, 1, &[C1])).muted_until, Patch::Unchanged);
    }

    #[test]
    fn a_direct_contact_has_its_one_to_one_chat_and_a_hidden_one_none() {
        let rows = events(&snapshot()).expect("events");
        let directs: Vec<&ConversationPatch> = conversations(&rows)
            .into_iter()
            .filter(|p| matches!(p.key, ConvKey::Direct(_)))
            .collect();
        // C0 DIRECT, C1 without a level (not hidden), C2 GROUP_OR_DELETED: none for C2.
        let keys: Vec<&ConvKey> = directs.iter().map(|p| &p.key).collect();
        assert_eq!(keys, vec![&direct(C0), &direct(C1)]);
        let hidden = &snapshot().contacts[2];
        assert_eq!(
            direct_patch(hidden),
            Ok(None),
            "a hidden contact's chat is stated"
        );
        // The whole patch of an archived, muted, protected chat, literally: nothing else is stated.
        let mut c = contact_policy(
            d2d_sync::contact::notification_trigger_policy_override::policy::NotificationTriggerPolicy::Never as i32,
            Some(1_761_400_000_000),
        );
        c.conversation_category = Some(ConversationCategory::Protected as i32);
        c.conversation_visibility = Some(ConversationVisibility::Archived as i32);
        c.nickname = Some("Nick J4B".to_owned());
        let mut expected = ConversationPatch::new(direct(C0));
        expected.features = Some(h2_features());
        expected.protected = Some(true);
        expected.archived = Some(true);
        expected.pinned_order = Patch::Clear;
        expected.muted_until = Patch::Set(1_761_400_000_000);
        assert_eq!(direct_of(&c), expected, "the 1:1 chat's patch differs");
        assert!(
            direct_patch(&d2d_sync::Contact {
                identity: "J4B C000".to_owned(),
                ..contact(C0)
            })
            .is_err()
        );
    }
}
