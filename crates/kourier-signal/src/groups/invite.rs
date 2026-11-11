//! The invite link of a group (copy-and-links). What stands is the server's
//! (`GET /v2/groups`); a change is one `PATCH /v2/groups` of the next revision, which the server
//! accepts from an administrator only. The link holds the group's master key and lets anyone
//! join: it is never a log field. Afterwards the group is fetched as after any change, so its
//! features say whether the link is on. Other members learn of a change at their next fetch:
//! Kourier sends them no group update message for it.

use std::rc::Rc;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{HeaderValue, Method, StatusCode};
use kourier_backend_api::Reply;
use kourier_model::{BackendError, ConvKey, ErrorKind, GroupInviteAction, Redacted};
use prost::Message as _;

use super::fetch::{self, Fetched};
use super::{ACCESS_ADMINISTRATOR, GroupRow, Membership, credentials};
use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::net::{HttpsHost, HttpsRequest, SignalTransport};
use crate::proto::{self, group_change::Actions, group_change::actions};

/// Anyone with the link joins (`AccessControl.AccessRequired.ANY`).
pub(crate) const ACCESS_ANY: i32 = 1;
/// Nobody joins by the link (`UNSATISFIABLE`).
const ACCESS_OFF: i32 = 4;
/// The server's size of an invite link password.
const PASSWORD_BYTES: usize = 16;

/// `Command::GroupInvite`: on a task of its own, so a slow server holds the account up in nothing.
pub(crate) async fn run<T: SignalTransport>(
    shared: Rc<Shared<T>>,
    group: ConvKey,
    action: GroupInviteAction,
    reply: Reply<Option<Redacted<String>>>,
) {
    let _ = reply.send(answer(&shared, &group, action).await);
}

async fn answer<T: SignalTransport>(
    shared: &Shared<T>,
    group: &ConvKey,
    action: GroupInviteAction,
) -> std::result::Result<Option<Redacted<String>>, BackendError> {
    let (Some(account), Some(socket)) = (shared.account(), shared.work_socket()) else {
        return Err(BackendError::transient("the account is not connected"));
    };
    let row = member_row(shared, group)?;
    let (link, changed) = ask(shared, &socket, &account, &row, action)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, ?action, "the invite link of a group was not answered");
            e.to_backend_error()
        })?;
    if changed && let Err(e) = super::refresh(shared, &socket, &account, &row).await {
        tracing::warn!(error = %e, "a group was not read again after its invite link changed");
    }
    Ok(link.map(Redacted::new))
}

/// The stored group, when we are a member of one that still stands.
fn member_row<T: SignalTransport>(
    shared: &Shared<T>,
    group: &ConvKey,
) -> std::result::Result<GroupRow, BackendError> {
    let id = super::id_of_conv(group)
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "not a Signal group key"))?;
    let row = shared
        .db
        .read(|c| super::load_by_id(c, &id))
        .map_err(|e| e.to_backend_error())?
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "unknown group"))?;
    if row.membership != Membership::Member || row.snapshot.as_ref().is_none_or(|s| s.terminated) {
        return Err(
            BackendError::new(ErrorKind::Unsupported, "not a member of the group")
                .with_user_message("You are not a member of this group."),
        );
    }
    Ok(row)
}

/// The link as it stands after `action`, and whether the group changed. Somebody else's change
/// between the read and the write (409) is met once by reading again.
async fn ask<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
    account: &Account,
    row: &GroupRow,
    action: GroupInviteAction,
) -> Result<(Option<String>, bool)> {
    let auth = credentials::authorization(shared, socket, account, &row.keys).await?;
    let mut again = true;
    loop {
        let mut state = match fetch::state(shared, &auth, None).await? {
            Fetched::Current { state, .. } => state,
            Fetched::NotMember | Fetched::Gone => {
                return Err(Error::Status {
                    what: "group change",
                    status: StatusCode::FORBIDDEN.as_u16(),
                });
            }
        };
        let Some(change) = change_for(&state, action, rand::random()) else {
            return Ok((link_of(&state, &row.keys.master), false));
        };
        if patch(shared, &auth, &change).await? {
            apply(&mut state, &change);
            return Ok((link_of(&state, &row.keys.master), true));
        }
        if !again {
            return Err(Error::Status {
                what: "group change",
                status: StatusCode::CONFLICT.as_u16(),
            });
        }
        again = false;
    }
}

/// `PATCH /v2/groups` of `change`: `false` when the revision was taken by another change.
async fn patch<T: SignalTransport>(
    shared: &Shared<T>,
    auth: &HeaderValue,
    change: &Actions,
) -> Result<bool> {
    const PROTOBUF: HeaderValue = HeaderValue::from_static("application/x-protobuf");
    let mut request = HttpsRequest::new(Method::PATCH, HttpsHost::Storage, "/v2/groups");
    request.timeout = fetch::REQUEST_TIMEOUT;
    let headers = &mut request.headers;
    headers.insert(http::header::AUTHORIZATION, auth.clone());
    headers.insert(http::header::ACCEPT, PROTOBUF);
    headers.insert(http::header::CONTENT_TYPE, PROTOBUF);
    request.body = Some(change.encode_to_vec().into());
    let response = shared.net.https(request).await?;
    match response.status {
        StatusCode::OK => Ok(true),
        StatusCode::CONFLICT => Ok(false),
        status => Err(fetch::status_error(shared, "group change", status)?),
    }
}

/// Who may join by the link as `group` states it.
fn access_of(group: &proto::Group) -> i32 {
    group
        .access_control
        .as_ref()
        .map_or(0, |a| a.add_from_invite_link)
}

fn is_on(group: &proto::Group) -> bool {
    let access = access_of(group);
    (access == ACCESS_ANY || access == ACCESS_ADMINISTRATOR)
        && !group.invite_link_password.is_empty()
}

/// The change `action` needs on `group`, `None` when there is nothing to change. A link that is
/// turned off keeps its password, so turning it on again brings the same link back (Reset is
/// what ends an old link); a link with approval stays so.
fn change_for(
    group: &proto::Group,
    action: GroupInviteAction,
    fresh: [u8; PASSWORD_BYTES],
) -> Option<Actions> {
    let password = || actions::ModifyInviteLinkPasswordAction {
        invite_link_password: fresh.to_vec(),
    };
    let access = |level| actions::ModifyAddFromInviteLinkAccessControlAction {
        add_from_invite_link_access: level,
    };
    let mut change = Actions {
        version: group.version + 1,
        ..Actions::default()
    };
    match action {
        GroupInviteAction::Read => return None,
        GroupInviteAction::Reset => change.modify_invite_link_password = Some(password()),
        GroupInviteAction::TurnOn if is_on(group) => return None,
        GroupInviteAction::TurnOn => {
            if access_of(group) != ACCESS_ANY && access_of(group) != ACCESS_ADMINISTRATOR {
                change.modify_add_from_invite_link_access = Some(access(ACCESS_ANY));
            }
            if group.invite_link_password.is_empty() {
                change.modify_invite_link_password = Some(password());
            }
        }
        GroupInviteAction::TurnOff if access_of(group) == ACCESS_OFF => return None,
        GroupInviteAction::TurnOff => {
            change.modify_add_from_invite_link_access = Some(access(ACCESS_OFF));
        }
    }
    Some(change)
}

/// `group` with `change` made, without asking the server again.
fn apply(group: &mut proto::Group, change: &Actions) {
    if let Some(access) = &change.modify_add_from_invite_link_access {
        group
            .access_control
            .get_or_insert_with(Default::default)
            .add_from_invite_link = access.add_from_invite_link_access;
    }
    if let Some(password) = &change.modify_invite_link_password {
        group
            .invite_link_password
            .clone_from(&password.invite_link_password);
    }
    group.version = change.version;
}

fn link_of(group: &proto::Group, master: &[u8; 32]) -> Option<String> {
    is_on(group).then(|| link(master, &group.invite_link_password))
}

/// `https://signal.group/#<base64url of GroupInviteLink>`, the form Signal's clients open.
fn link(master: &[u8; 32], password: &[u8]) -> String {
    let contents = proto::group_invite_link::GroupInviteLinkContentsV1 {
        group_master_key: master.to_vec(),
        invite_link_password: password.to_vec(),
    };
    let bytes = proto::GroupInviteLink {
        contents: Some(proto::group_invite_link::Contents::ContentsV1(contents)),
    }
    .encode_to_vec();
    format!("https://signal.group/#{}", URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRESH: [u8; PASSWORD_BYTES] = [7; PASSWORD_BYTES];

    fn group(access: i32, password: &[u8]) -> proto::Group {
        proto::Group {
            version: 41,
            access_control: Some(proto::AccessControl {
                add_from_invite_link: access,
                ..proto::AccessControl::default()
            }),
            invite_link_password: password.to_vec(),
            ..proto::Group::default()
        }
    }

    fn access_in(change: &Actions) -> Option<i32> {
        change
            .modify_add_from_invite_link_access
            .as_ref()
            .map(|a| a.add_from_invite_link_access)
    }

    fn password_in(change: &Actions) -> Option<&[u8]> {
        change
            .modify_invite_link_password
            .as_ref()
            .map(|p| &p.invite_link_password[..])
    }

    #[test]
    fn the_link_holds_the_key_and_the_password() {
        let made = link(&[9; 32], &FRESH);
        let code = made.strip_prefix("https://signal.group/#").expect("form");
        let bytes = URL_SAFE_NO_PAD.decode(code).expect("base64url");
        let decoded = proto::GroupInviteLink::decode(&bytes[..]).expect("proto");
        let Some(proto::group_invite_link::Contents::ContentsV1(contents)) = decoded.contents
        else {
            panic!("version 1");
        };
        assert_eq!(contents.group_master_key, [9; 32]);
        assert_eq!(contents.invite_link_password, FRESH);
    }

    #[test]
    fn reading_changes_nothing_and_a_link_that_is_off_is_none() {
        for on in [group(ACCESS_ANY, &[1; 16]), group(ACCESS_OFF, &[1; 16])] {
            assert!(change_for(&on, GroupInviteAction::Read, FRESH).is_none());
        }
        assert!(link_of(&group(ACCESS_ANY, &[1; 16]), &[9; 32]).is_some());
        assert!(link_of(&group(ACCESS_ADMINISTRATOR, &[1; 16]), &[9; 32]).is_some());
        assert!(link_of(&group(ACCESS_OFF, &[1; 16]), &[9; 32]).is_none());
        assert!(link_of(&group(ACCESS_ANY, &[]), &[9; 32]).is_none());
    }

    #[test]
    fn a_reset_is_a_new_password_of_the_next_revision_and_nothing_else() {
        let change = change_for(
            &group(ACCESS_ANY, &[1; 16]),
            GroupInviteAction::Reset,
            FRESH,
        )
        .expect("a change");
        assert_eq!(change.version, 42);
        assert_eq!(password_in(&change), Some(&FRESH[..]));
        assert_eq!(access_in(&change), None);
    }

    #[test]
    fn turning_on_makes_a_password_only_when_there_is_none() {
        let first = change_for(&group(ACCESS_OFF, &[]), GroupInviteAction::TurnOn, FRESH)
            .expect("a change");
        assert_eq!(access_in(&first), Some(ACCESS_ANY));
        assert_eq!(password_in(&first), Some(&FRESH[..]));

        let again = change_for(
            &group(ACCESS_OFF, &[1; 16]),
            GroupInviteAction::TurnOn,
            FRESH,
        )
        .expect("a change");
        assert_eq!(access_in(&again), Some(ACCESS_ANY));
        assert_eq!(password_in(&again), None, "the same link comes back");

        assert!(
            change_for(
                &group(ACCESS_ANY, &[1; 16]),
                GroupInviteAction::TurnOn,
                FRESH
            )
            .is_none()
        );
    }

    #[test]
    fn turning_off_keeps_the_password_and_a_link_already_off_needs_no_change() {
        let change = change_for(
            &group(ACCESS_ANY, &[1; 16]),
            GroupInviteAction::TurnOff,
            FRESH,
        )
        .expect("a change");
        assert_eq!(access_in(&change), Some(ACCESS_OFF));
        assert_eq!(password_in(&change), None);
        assert!(
            change_for(
                &group(ACCESS_OFF, &[1; 16]),
                GroupInviteAction::TurnOff,
                FRESH
            )
            .is_none()
        );
    }

    #[test]
    fn a_change_made_shows_in_the_link() {
        let mut state = group(ACCESS_OFF, &[]);
        let change = change_for(&state, GroupInviteAction::TurnOn, FRESH).expect("a change");
        apply(&mut state, &change);
        assert_eq!(state.version, 42);
        assert_eq!(link_of(&state, &[9; 32]), Some(link(&[9; 32], &FRESH)));
        let change = change_for(&state, GroupInviteAction::TurnOff, FRESH).expect("a change");
        apply(&mut state, &change);
        assert_eq!(link_of(&state, &[9; 32]), None);
    }
}
