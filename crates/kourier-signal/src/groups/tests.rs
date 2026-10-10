//! Groups without a server: keys and ids, the snapshot format, the core's member list and the
//! group conversation features.

use kourier_model::{CannotSend, ConvKey, FormatKind, MemberRole};
use libsignal_core::Aci;
use uuid::Uuid;

use super::apply::member_list;
use super::{
    GroupKeys, Membership, SnapMember, SnapPending, Snapshot, conv_key_of_id, job_key,
    parse_job_key, system_local,
};
use crate::features::group_features;

fn keys() -> GroupKeys {
    GroupKeys::from_master([7; 32])
}

#[test]
fn keys_ids_and_lanes_are_stable_and_never_show_the_master_key() {
    let a = keys();
    let b = GroupKeys::from_slice(&[7; 32]).expect("32 bytes");
    assert_eq!(a.id, b.id);
    assert!(GroupKeys::from_slice(&[7; 31]).is_none());
    // The core's key is built from the group id, never the master key.
    let ConvKey::Group(key) = a.conv_key() else {
        panic!("a group key");
    };
    assert!(key.starts_with("gv2:"));
    assert_eq!(a.conv_key(), conv_key_of_id(&a.id));
    assert!(a.conv_key().validate().is_ok());
    assert_eq!(a.lane(), crate::receive::group_lane(&[7; 32]));
    assert!(!format!("{a:?}").contains("0707"));
    // Refresh job keys round-trip; malformed ones do not parse.
    assert_eq!(parse_job_key(&job_key(&a.id)).as_deref(), Some(&a.id[..]));
    assert_eq!(parse_job_key("abc"), None);
    assert_eq!(parse_job_key("zz"), None);
    // System message ids name group, revision and index.
    let local = system_local(&a.id, 3, 1);
    assert!(local.starts_with("gv2:") && local.ends_with(":3:1"));
    assert_ne!(local, system_local(&a.id, 3, 0));
    assert_ne!(
        local,
        system_local(&GroupKeys::from_master([8; 32]).id, 3, 1)
    );
}

fn member(n: u128, admin: bool) -> SnapMember {
    SnapMember {
        aci: Uuid::from_u128(n),
        admin,
        joined_at: 0,
    }
}

fn snapshot() -> Snapshot {
    Snapshot {
        revision: 4,
        title: "Climbing".to_owned(),
        members: vec![member(1, true), member(2, false)],
        pending: vec![SnapPending {
            service_id: Uuid::from_u128(3).to_string(),
            admin: false,
        }],
        requesting: vec![Uuid::from_u128(4)],
        ..Snapshot::default()
    }
}

#[test]
fn snapshots_round_trip_and_answer_membership() {
    let snapshot = snapshot();
    let parsed = Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
    assert_eq!(parsed, snapshot);
    assert!(parsed.is_member(Aci::from(Uuid::from_u128(1))));
    assert!(!parsed.is_member(Aci::from(Uuid::from_u128(3))));
    assert!(parsed.is_pending(&Uuid::from_u128(3).to_string()));
    assert!(Snapshot::from_json(b"not json").is_err());
}

#[test]
fn the_member_list_is_a_full_snapshot_with_roles() {
    let list = member_list(&snapshot());
    assert!(list.full);
    let roles: Vec<MemberRole> = list.upsert.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            MemberRole::Admin,
            MemberRole::Member,
            MemberRole::Pending,
            MemberRole::Requesting
        ]
    );
}

#[test]
fn group_features_say_why_we_cannot_send() {
    let fixture = crate::receive::tests::fixture();
    let account = &fixture.account;
    let mut snapshot = snapshot();
    snapshot.members.push(SnapMember {
        aci: account.aci.into(),
        admin: false,
        joined_at: 0,
    });
    let limits = crate::features::Limits::with_max_bytes(1000);
    let member = group_features(Some(&snapshot), Membership::Member, account, &limits);
    assert_eq!(member.format_support(FormatKind::Mention).level(), 2);
    assert_eq!(member.cannot_send, None, "members write to the group (S11)");
    assert_eq!(
        group_features(Some(&snapshot), Membership::Left, account, &limits).cannot_send,
        Some(CannotSend::NotMember)
    );
    snapshot.announcements_only = true;
    assert_eq!(
        group_features(Some(&snapshot), Membership::Member, account, &limits).cannot_send,
        Some(CannotSend::AdminsOnly)
    );
    assert_eq!(
        group_features(None, Membership::Gone, account, &limits).cannot_send,
        Some(CannotSend::NotMember)
    );
}
