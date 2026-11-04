//! THE GROUP NOTICES of a reflected `GroupSync.Update` (TH5 A7a; `md-d2d.proto` `GroupSync.Update`, the
//! decisions' L6): the `Body::System` rows that say what changed, in ONE fixed order, members SORTED by
//! identity (the spec's `member_state_changes` is a protobuf map, a `HashMap` in prost: never its order):
//!
//! 1. `MembersAdded { by, users }`: every `ADDED` identity NOT in the stored members (the spec's step 4.1),
//!    and the user when its own state went back to `MEMBER`;
//! 2. `MembersRemoved { by, users }`: every `KICKED` identity that IS in the stored members (step 4.2), and
//!    the user when its own state became `KICKED`;
//! 3. `MemberLeft { user }`, one per `LEFT` identity that is in the stored members (sorted), and the user
//!    when its own state became `LEFT`;
//! 4. `TitleChanged { by, title }` when the delta's name differs from the stored one (an EMPTY name too: the
//!    viewmodel says "removed the title"; Desktop's `GROUP_NAME_CHANGED` for `oldName !== newName`);
//! 5. `AvatarChanged { by }` when the delta carries a profile picture (set or removed, as Desktop's
//!    `GROUP_PROFILE_PICTURE_CHANGED`).
//!
//! `by` = the creator (only the creator manages a Threema group), or `None` when the creator is the user.
//! The own ID and the creator are never taken from `member_state_changes` (the spec's member list holds
//! neither; the own state is `user_state`'s). WHAT `GroupSync.Create` SHOWS: NO notice (as Desktop's
//! `_createGroupFromGroupSyncCreate`): the chat appears with its title and members. A Create of a group the
//! file holds updates its fields with no notice either.
//!
//! THE ROWS: `local` = `gs:<the first 8 bytes of the envelope's hashed D2D nonce, 16 hex>:<index>` (stable
//! on a redelivery, which repeats the nonce; unique per notice); `origin` = `Remote { author: creator }`, or
//! `OwnOtherDevice` when the creator is the user or the notice is the user's own leave; `sent_at` and
//! `server_at` = the reflection's time; `silent` (a notice never notifies); `catch_up` as the connection
//! says. Pure.

use std::collections::HashMap;
use std::fmt::Write as _;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::{Body, ConvKey, Origin, SystemEvent, UserId};
use libthreema::protobuf::d2d::group_sync::update::MemberStateChange;
use libthreema::protobuf::d2d_sync::{self, group::UserState};

use super::map::Cx;
use crate::join::events::user;

/// What an update changed, read against the stored group.
pub(crate) struct Change<'a> {
    /// The stored group BEFORE the update (`current` of the spec).
    pub(crate) old: &'a d2d_sync::Group,
    /// The group after it.
    pub(crate) new: &'a d2d_sync::Group,
    pub(crate) creator: &'a str,
    pub(crate) own: &'a str,
    /// The update's `member_state_changes` (validated: IDs and enum values).
    pub(crate) changes: &'a HashMap<String, i32>,
    /// The new name when it differs from the stored one.
    pub(crate) title: Option<&'a str>,
    /// The update carried a profile picture.
    pub(crate) picture: bool,
}

fn state(group: &d2d_sync::Group) -> UserState {
    group
        .user_state
        .and_then(|s| UserState::try_from(s).ok())
        .unwrap_or(UserState::Member)
}

/// The notices of `change`, in the module doc's order (members sorted by identity). An ID that is no
/// valid user is left out (validated before; a belt).
pub(crate) fn group_notices(change: &Change<'_>) -> Vec<SystemEvent> {
    let current: &[String] = change
        .old
        .member_identities
        .as_ref()
        .map_or(&[][..], |m| m.identities.as_slice());
    let mut added: Vec<&str> = Vec::new();
    let mut kicked: Vec<&str> = Vec::new();
    let mut left: Vec<&str> = Vec::new();
    for (identity, value) in change.changes {
        let id = identity.as_str();
        if id == change.own || id == change.creator {
            continue;
        }
        let known = current.iter().any(|m| m == id);
        match MemberStateChange::try_from(*value) {
            Ok(MemberStateChange::Added) if !known => added.push(id),
            Ok(MemberStateChange::Kicked) if known => kicked.push(id),
            Ok(MemberStateChange::Left) if known => left.push(id),
            _ => {}
        }
    }
    // The user's own state (`user_state`, never the map).
    match (state(change.old), state(change.new)) {
        (UserState::Kicked | UserState::Left, UserState::Member) => added.push(change.own),
        (UserState::Member, UserState::Kicked) => kicked.push(change.own),
        (UserState::Member, UserState::Left) => left.push(change.own),
        _ => {}
    }
    for list in [&mut added, &mut kicked, &mut left] {
        list.sort_unstable();
        list.dedup();
    }
    let by = if change.creator == change.own {
        None
    } else {
        user(change.creator).ok()
    };
    let users =
        |ids: &[&str]| -> Vec<UserId> { ids.iter().filter_map(|id| user(id).ok()).collect() };
    let mut out = Vec::new();
    if !added.is_empty() {
        out.push(SystemEvent::MembersAdded {
            by: by.clone(),
            users: users(&added),
        });
    }
    if !kicked.is_empty() {
        out.push(SystemEvent::MembersRemoved {
            by: by.clone(),
            users: users(&kicked),
        });
    }
    for id in left {
        if let Ok(user) = user(id) {
            out.push(SystemEvent::MemberLeft { user });
        }
    }
    if let Some(title) = change.title {
        out.push(SystemEvent::TitleChanged {
            by: by.clone(),
            title: title.trim().to_owned(),
        });
    }
    if change.picture {
        out.push(SystemEvent::AvatarChanged { by });
    }
    out
}

/// The notice id: `gs:<16 hex of the first 8 bytes of the hashed nonce>:<index>`.
pub(crate) fn notice_id(tag: &[u8; 8], index: usize) -> String {
    let mut text = String::from("gs:");
    for byte in tag {
        let _ = write!(text, "{byte:02x}");
    }
    let _ = write!(text, ":{index}");
    text
}

/// The `Message` rows of `notices` in `conv` (module doc).
pub(crate) fn rows(
    notices: Vec<SystemEvent>,
    conv: &ConvKey,
    creator: &str,
    cx: &Cx<'_>,
) -> Vec<BackendEvent> {
    let at = i64::try_from(cx.server_at).unwrap_or(cx.now_ms);
    let own = cx.own;
    notices
        .into_iter()
        .enumerate()
        .map(|(index, notice)| {
            let own_leave = matches!(&notice, SystemEvent::MemberLeft { user: u }
                if user(own).is_ok_and(|me| me == *u));
            let origin = match user(creator) {
                Ok(author) if creator != own && !own_leave => Origin::Remote {
                    author,
                    device: None,
                },
                _ => Origin::OwnOtherDevice { device: None },
            };
            let mut message = IncomingMessage::new(
                conv.clone(),
                notice_id(&cx.notice_tag, index),
                origin,
                Body::System(notice),
                at,
                cx.now_ms,
            );
            message.server_at = Some(at);
            message.silent = true;
            message.catch_up = cx.catch_up;
            BackendEvent::Message(message)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use libthreema::protobuf::common::Identities;

    use super::*;

    const OWN: &str = "A7NOTOWN";
    const CREATOR: &str = "A7NOTCRE";
    const A: &str = "A7NOT00A";
    const B: &str = "A7NOT00B";
    const C: &str = "A7NOT00C";
    const D: &str = "A7NOT00D";

    fn u(id: &str) -> UserId {
        user(id).expect("a user")
    }

    fn grp(ids: &[&str], state: UserState) -> d2d_sync::Group {
        d2d_sync::Group {
            member_identities: Some(Identities {
                identities: ids.iter().map(|i| (*i).to_owned()).collect(),
            }),
            user_state: Some(state as i32),
            ..Default::default()
        }
    }

    fn changes(list: &[(&str, MemberStateChange)]) -> HashMap<String, i32> {
        list.iter()
            .map(|(id, c)| ((*id).to_owned(), *c as i32))
            .collect()
    }

    #[test]
    fn every_kind_in_its_order_with_sorted_members_and_the_spec_s_step_4() {
        let old = grp(&[C, A, B], UserState::Member);
        let new = grp(&[A, D], UserState::Member);
        // D added (new), B kicked, C left; A "added" but already a member: no notice (step 4.1); an
        // unknown member "kicked": no notice (step 4.2); the own ID and the creator in the map: ignored.
        let map = changes(&[
            (D, MemberStateChange::Added),
            (A, MemberStateChange::Added),
            (C, MemberStateChange::Left),
            (B, MemberStateChange::Kicked),
            ("A7NOT00Z", MemberStateChange::Kicked),
            (OWN, MemberStateChange::Kicked),
            (CREATOR, MemberStateChange::Left),
        ]);
        let got = group_notices(&Change {
            old: &old,
            new: &new,
            creator: CREATOR,
            own: OWN,
            changes: &map,
            title: Some("New title"),
            picture: true,
        });
        let by = Some(u(CREATOR));
        assert_eq!(
            got,
            vec![
                SystemEvent::MembersAdded {
                    by: by.clone(),
                    users: vec![u(D)]
                },
                SystemEvent::MembersRemoved {
                    by: by.clone(),
                    users: vec![u(B)]
                },
                SystemEvent::MemberLeft { user: u(C) },
                SystemEvent::TitleChanged {
                    by: by.clone(),
                    title: "New title".to_owned()
                },
                SystemEvent::AvatarChanged { by },
            ]
        );
        // Several of one kind: sorted by identity whatever the map's order.
        let old = grp(&[A, B, C], UserState::Member);
        let map = changes(&[
            (C, MemberStateChange::Kicked),
            (A, MemberStateChange::Kicked),
            (B, MemberStateChange::Left),
        ]);
        let got = group_notices(&Change {
            old: &old,
            new: &grp(&[], UserState::Member),
            creator: OWN,
            own: OWN,
            changes: &map,
            title: None,
            picture: false,
        });
        assert_eq!(
            got,
            vec![
                SystemEvent::MembersRemoved {
                    by: None,
                    users: vec![u(A), u(C)]
                },
                SystemEvent::MemberLeft { user: u(B) },
            ],
            "the user's own group: by None, members sorted"
        );
    }

    #[test]
    fn the_own_state_is_a_notice_of_its_kind() {
        let notice = |from, to| {
            group_notices(&Change {
                old: &grp(&[A], from),
                new: &grp(&[A], to),
                creator: CREATOR,
                own: OWN,
                changes: &HashMap::new(),
                title: None,
                picture: false,
            })
        };
        let by = Some(u(CREATOR));
        assert_eq!(
            notice(UserState::Member, UserState::Kicked),
            vec![SystemEvent::MembersRemoved {
                by: by.clone(),
                users: vec![u(OWN)]
            }]
        );
        assert_eq!(
            notice(UserState::Member, UserState::Left),
            vec![SystemEvent::MemberLeft { user: u(OWN) }]
        );
        assert_eq!(
            notice(UserState::Kicked, UserState::Member),
            vec![SystemEvent::MembersAdded {
                by,
                users: vec![u(OWN)]
            }]
        );
        assert!(notice(UserState::Member, UserState::Member).is_empty());
    }

    #[test]
    fn the_rows_have_stable_ids_and_their_origin() {
        let conv = crate::join::events::group_key(CREATOR, 7).expect("a key");
        let cx = Cx {
            own: OWN,
            now_ms: 9,
            server_at: 5,
            catch_up: true,
            notice_tag: [0xab, 1, 2, 3, 4, 5, 6, 0xff],
            remap: false,
        };
        let notices = vec![
            SystemEvent::MemberLeft { user: u(A) },
            SystemEvent::MemberLeft { user: u(OWN) },
        ];
        let rows = rows(notices.clone(), &conv, CREATOR, &cx);
        let again = super::rows(notices, &conv, CREATOR, &cx);
        assert_eq!(rows, again, "the same envelope gives the same rows");
        let msgs: Vec<&IncomingMessage> = rows
            .iter()
            .map(|e| match e {
                BackendEvent::Message(m) => m,
                _ => panic!("a notice is a message"),
            })
            .collect();
        assert_eq!(&*msgs[0].local, "gs:ab010203040506ff:0");
        assert_eq!(&*msgs[1].local, "gs:ab010203040506ff:1");
        assert_eq!(
            msgs[0].origin,
            Origin::Remote {
                author: u(CREATOR),
                device: None
            }
        );
        assert_eq!(msgs[1].origin, Origin::OwnOtherDevice { device: None });
        assert!(msgs.iter().all(|m| m.silent
            && m.catch_up
            && m.sent_at == 5
            && m.server_at == Some(5)
            && m.received_at == 9
            && m.conv == conv));
    }
}
