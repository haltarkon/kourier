//! Decrypting group states and changes (`SD/ts/groups.preload.ts` `decryptGroupState` 7195,
//! `decryptMember` 7379, `decryptGroupChange` 6428): titles, descriptions, timers and avatars
//! are `GroupAttributeBlob`s under the group's blob key (with padding), user ids are
//! `UuidCiphertext`s, profile keys `ProfileKeyCiphertext`s bound to the member's ACI. Roles,
//! access rules and revisions are plaintext. A member whose id does not decrypt is skipped, as
//! Desktop does.

use std::time::Duration;

use kourier_model::{SystemEvent, UserId};
use libsignal_core::{Aci, ServiceId};
use prost::Message as _;
use zkgroup::groups::{ProfileKeyCiphertext, UuidCiphertext};

use super::{Access, GroupKeys, SUPPORTED_CHANGE_EPOCH, SnapMember, SnapPending, Snapshot};
use crate::proto::{self, group_attribute_blob::Content as Blob};
use crate::service_id::{aci_user_id, service_id_user_id};

/// `Member.Role.ADMINISTRATOR`.
const ROLE_ADMINISTRATOR: i32 = 2;

/// A decrypted group state and the profile keys of its members.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DecodedState {
    /// The state.
    pub(crate) snapshot: Snapshot,
    /// `(member, profile key)` for every member whose key decrypted.
    pub(crate) profile_keys: Vec<(Aci, [u8; 32])>,
}

/// What one change did, as timeline events.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DecodedChange {
    /// The revision the change produced.
    pub(crate) revision: u32,
    /// Who made it (`None` when the source did not decrypt).
    pub(crate) source: Option<ServiceId>,
    /// The system events it implies, in action order.
    pub(crate) events: Vec<SystemEvent>,
}

fn attribute(keys: &GroupKeys, ciphertext: &[u8]) -> Option<Blob> {
    if ciphertext.is_empty() {
        return None;
    }
    let plaintext = keys.secret.decrypt_blob_with_padding(ciphertext).ok()?;
    proto::GroupAttributeBlob::decode(&plaintext[..])
        .ok()?
        .content
}

/// Decrypts a `UuidCiphertext`.
pub(crate) fn service_id(keys: &GroupKeys, ciphertext: &[u8]) -> Option<ServiceId> {
    let ciphertext: UuidCiphertext = zkgroup::deserialize(ciphertext).ok()?;
    keys.secret.decrypt_service_id(ciphertext).ok()
}

fn aci(keys: &GroupKeys, ciphertext: &[u8]) -> Option<Aci> {
    match service_id(keys, ciphertext)? {
        ServiceId::Aci(aci) => Some(aci),
        ServiceId::Pni(_) => None,
    }
}

fn profile_key(keys: &GroupKeys, ciphertext: &[u8], aci: Aci) -> Option<[u8; 32]> {
    if ciphertext.is_empty() {
        return None;
    }
    let ciphertext: ProfileKeyCiphertext = zkgroup::deserialize(ciphertext).ok()?;
    keys.secret
        .decrypt_profile_key(ciphertext, aci)
        .ok()
        .map(|key| key.get_bytes())
}

/// The title of an encrypted title blob in a group state ("" when there is none, or when it
/// does not decrypt: the state then has no name to show). A title CHANGE does not read it this
/// way: see [`changed_title`].
pub(crate) fn title(keys: &GroupKeys, ciphertext: &[u8]) -> String {
    match attribute(keys, ciphertext) {
        Some(Blob::Title(title)) => title,
        _ => String::new(),
    }
}

/// The new title of a `modifyTitle` action, as Desktop reads it (`decryptGroupChange`): an
/// action without a blob, or with a blob of an empty title, removed the name (`Some("")`); a
/// blob that is there and does not decrypt, belongs to another group or is no title says
/// nothing about the name, and the action is skipped (`None`), like a member whose id does not
/// decrypt: "removed the group name" for it would be false.
fn changed_title(keys: &GroupKeys, ciphertext: &[u8]) -> Option<String> {
    if ciphertext.is_empty() {
        return Some(String::new());
    }
    match attribute(keys, ciphertext)? {
        Blob::Title(title) => Some(title),
        _ => None,
    }
}

/// The timer of an encrypted timer blob (0 = off).
fn timer(keys: &GroupKeys, ciphertext: &[u8]) -> u32 {
    match attribute(keys, ciphertext) {
        Some(Blob::DisappearingMessagesDuration(secs)) => secs,
        _ => 0,
    }
}

/// The image bytes of a downloaded avatar blob.
pub(crate) fn avatar(keys: &GroupKeys, ciphertext: &[u8]) -> Option<Vec<u8>> {
    match attribute(keys, ciphertext)? {
        Blob::Avatar(bytes) if !bytes.is_empty() => Some(bytes),
        _ => None,
    }
}

/// Decrypts a group state.
pub(crate) fn state(keys: &GroupKeys, group: &proto::Group) -> DecodedState {
    let mut profile_keys = Vec::new();
    let mut members = Vec::with_capacity(group.members.len());
    for member in &group.members {
        let Some(aci) = aci(keys, &member.user_id) else {
            continue;
        };
        if let Some(key) = profile_key(keys, &member.profile_key, aci) {
            profile_keys.push((aci, key));
        }
        members.push(SnapMember {
            aci: aci.into(),
            admin: member.role == ROLE_ADMINISTRATOR,
            joined_at: member.joined_at_version,
        });
    }
    let pending = group
        .members_pending_profile_key
        .iter()
        .filter_map(|pending| {
            let member = pending.member.as_ref()?;
            let service_id = service_id(keys, &member.user_id)?;
            Some(SnapPending {
                service_id: service_id.service_id_string(),
                admin: member.role == ROLE_ADMINISTRATOR,
            })
        })
        .collect();
    let requesting = group
        .members_pending_admin_approval
        .iter()
        .filter_map(|requesting| aci(keys, &requesting.user_id).map(Into::into))
        .collect();
    let description = match attribute(keys, &group.description) {
        Some(Blob::DescriptionText(text)) if !text.is_empty() => Some(text),
        _ => None,
    };
    let access = group
        .access_control
        .as_ref()
        .map(|access| Access {
            attributes: access.attributes,
            members: access.members,
            add_from_invite_link: access.add_from_invite_link,
        })
        .unwrap_or_default();
    DecodedState {
        snapshot: Snapshot {
            revision: group.version,
            title: title(keys, &group.title),
            description,
            avatar: Some(group.avatar_url.clone()).filter(|url| !url.is_empty()),
            timer_secs: timer(keys, &group.disappearing_messages_timer),
            members,
            pending,
            requesting,
            announcements_only: group.announcements_only,
            terminated: group.terminated,
            access,
        },
        profile_keys,
    }
}

/// `by` of a system event: `None` for our own account (the viewmodel shows "You"), otherwise
/// the source.
fn by(source: ServiceId, own: Aci) -> Option<UserId> {
    if source == ServiceId::from(own) {
        return None;
    }
    service_id_user_id(source).ok()
}

/// The users of a list of ACI ciphertexts that decrypt.
fn users(keys: &GroupKeys, ciphertexts: impl IntoIterator<Item = Vec<u8>>) -> Vec<UserId> {
    ciphertexts
        .into_iter()
        .filter_map(|ciphertext| aci(keys, &ciphertext))
        .filter_map(|aci| aci_user_id(aci).ok())
        .collect()
}

/// The timeline events of one change's actions (`SD/ts/groupChange.std.ts` renders the same
/// actions): members added (including invitations accepted and join requests approved), members
/// removed or leaving, title, avatar, description and timer changes. Role, access, invite-link,
/// ban and label changes have no system event in the contract yet and only update the state.
/// A change whose source does not decrypt yields no events (`by: None` would claim it was us).
pub(crate) fn actions(
    keys: &GroupKeys,
    actions: &proto::group_change::Actions,
    own: Aci,
) -> DecodedChange {
    let source = service_id(keys, &actions.source_user_id);
    let mut change = DecodedChange {
        revision: actions.version,
        source,
        events: Vec::new(),
    };
    let Some(source) = source else {
        return change;
    };
    let by = by(source, own);
    let events = &mut change.events;

    let added: Vec<Vec<u8>> = actions
        .add_members
        .iter()
        .filter_map(|add| add.added.as_ref().map(|m| m.user_id.clone()))
        .chain(
            actions
                .promote_members_pending_admin_approval
                .iter()
                .map(|p| p.user_id.clone()),
        )
        .collect();
    let added = users(keys, added);
    let source_user = service_id_user_id(source).ok();
    let joined_by_link = actions
        .add_members
        .iter()
        .any(|add| add.join_from_invite_link);
    if added.len() == 1
        && source_user.as_ref() == added.first()
        && source == ServiceId::from(own)
        && joined_by_link
    {
        events.push(SystemEvent::Joined);
    } else if !added.is_empty() {
        events.push(SystemEvent::MembersAdded {
            by: by.clone(),
            users: added,
        });
    }
    // Invitations accepted, and people who held an invitation promoted. The history import
    // makes the same notices for the same changes (`transfer::map`, Android's converter
    // writes them so): our own acceptance is a join of ours; a member who accepted is
    // "X joined" (`by` is X); and a member whom somebody ELSE promoted (an admin adding a
    // person who held an invitation) was added by that editor, not by themselves.
    let accepted = users(
        keys,
        actions
            .promote_members_pending_profile_key
            .iter()
            .map(|p| p.user_id.clone())
            .chain(
                actions
                    .promote_members_pending_pni_aci_profile_key
                    .iter()
                    .map(|p| p.user_id.clone()),
            ),
    );
    for user in accepted {
        if source_user.as_ref() != Some(&user) {
            events.push(SystemEvent::MembersAdded {
                by: by.clone(),
                users: vec![user],
            });
        } else if source == ServiceId::from(own) {
            events.push(SystemEvent::Joined);
        } else {
            events.push(SystemEvent::MembersAdded {
                by: Some(user.clone()),
                users: vec![user],
            });
        }
    }
    let removed = users(
        keys,
        actions
            .delete_members
            .iter()
            .map(|d| d.deleted_user_id.clone()),
    );
    let (left, removed): (Vec<UserId>, Vec<UserId>) = removed
        .into_iter()
        .partition(|user| source_user.as_ref() == Some(user));
    for user in left {
        events.push(SystemEvent::MemberLeft { user });
    }
    if !removed.is_empty() {
        events.push(SystemEvent::MembersRemoved {
            by: by.clone(),
            users: removed,
        });
    }
    if let Some(modify) = &actions.modify_title
        && let Some(title) = changed_title(keys, &modify.title)
    {
        events.push(SystemEvent::TitleChanged {
            by: by.clone(),
            title,
        });
    }
    if actions.modify_avatar.is_some() {
        events.push(SystemEvent::AvatarChanged { by: by.clone() });
    }
    if actions.modify_description.is_some() {
        events.push(SystemEvent::DescriptionChanged { by: by.clone() });
    }
    if let Some(modify) = &actions.modify_disappearing_message_timer {
        let secs = timer(keys, &modify.timer);
        events.push(SystemEvent::TimerChanged {
            by,
            timer: (secs > 0).then(|| Duration::from_secs(u64::from(secs))),
        });
    }
    change
}

/// The decoded events of a `GroupChange` from the server's log (trusted: it came over TLS from
/// the group server, so its signature is not checked, as in Desktop).
pub(crate) fn logged_change(
    keys: &GroupKeys,
    change: &proto::GroupChange,
    own: Aci,
) -> Option<DecodedChange> {
    if change.actions.is_empty() {
        return None;
    }
    let decoded = proto::group_change::Actions::decode(&change.actions[..]).ok()?;
    Some(actions(keys, &decoded, own))
}

/// The actions of a `GroupChange` embedded in a `DataMessage.groupV2` (untrusted: it came from
/// another user): the notary signature must verify against the server's public params, the
/// change epoch must be one we understand and the group id, when present, must be ours
/// (`groups.preload.ts` `getGroupUpdates`, `verifyNotarySignature`).
pub(crate) fn embedded_change(
    server: &zkgroup::ServerPublicParams,
    keys: &GroupKeys,
    bytes: &[u8],
) -> Option<proto::group_change::Actions> {
    let change = proto::GroupChange::decode(bytes).ok()?;
    if change.change_epoch > SUPPORTED_CHANGE_EPOCH {
        return None;
    }
    let signature: zkgroup::NotarySignatureBytes =
        change.server_signature.as_slice().try_into().ok()?;
    server.verify_signature(&change.actions, signature).ok()?;
    let actions = proto::group_change::Actions::decode(&change.actions[..]).ok()?;
    if !actions.group_id.is_empty() && actions.group_id[..] != keys.id[..] {
        return None;
    }
    Some(actions)
}
