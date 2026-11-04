//! W6a2, chunk B4: the group map. Upstream's own builders (`GroupMetadata::new`, `GroupParticipant::new`) make the
//! answers; every value is synthetic (the fictitious number range, no person).

use kourier_model::{CannotSend, MemberRole, Patch, UserId};
use whatsapp_rust::{GroupEphemeralSettings, GroupParticipant, ParticipantType};

use super::*;

const GROUP: &str = "120363000000000071@g.us";
const PARENT: &str = "120363000000000072@g.us";
const OUR_LID: &str = "99900100171";
const OUR_PN: &str = "15550100171";
const ADA_LID: &str = "99000000172";
const ADA_PN: &str = "31000000172";
const BOB_LID: &str = "99000000173";
const CY_PN: &str = "31000000174";
/// 2027-01-15, in ms: the fetch's time.
const AT_MS: i64 = 1_800_000_000_000;
const AT_SECS: u32 = 1_800_000_000;

fn own() -> OwnIds {
    OwnIds {
        lid: Some(UserId::from_parts("lid", OUR_LID).unwrap()),
        pn: Some(UserId::from_parts("pn", OUR_PN).unwrap()),
    }
}

fn fetch(own: &OwnIds) -> Fetch<'_> {
    Fetch { own, at_ms: AT_MS }
}

fn jid(text: &str) -> Jid {
    text.parse().expect("jid")
}

fn lid(user: &str) -> UserId {
    UserId::from_parts("lid", user).unwrap()
}

fn pn(user: &str) -> UserId {
    UserId::from_parts("pn", user).unwrap()
}

fn conv() -> ConvKey {
    group_conv(&jid(GROUP)).expect("a group")
}

/// A participant of `kind` named by `jid`, optionally with his number.
fn who(text: &str, kind: ParticipantType, number: Option<&str>) -> GroupParticipant {
    let mut participant = GroupParticipant::new(jid(text));
    participant.participant_type = kind;
    participant.phone_number = number.map(jid);
    participant
}

/// A LID group with Ada (admin) and Bob, and us as a plain member.
fn metadata() -> GroupMetadata {
    let mut metadata = GroupMetadata::new(jid(GROUP));
    metadata.subject = Some("Synthetic group".into());
    metadata.subject_time = Some(1_700_000_100);
    metadata.participants = vec![
        who(
            &format!("{ADA_LID}@lid"),
            ParticipantType::Admin,
            Some(&format!("{ADA_PN}@s.whatsapp.net")),
        ),
        who(&format!("{BOB_LID}@lid"), ParticipantType::Member, None),
        who(&format!("{OUR_LID}@lid"), ParticipantType::Member, None),
    ];
    metadata
}

fn found(metadata: GroupMetadata) -> GroupMetadataResult {
    GroupMetadataResult::Found(Box::new(metadata))
}

fn outcome(result: &GroupMetadataResult, stored: &State) -> Outcome {
    let own = own();
    map(result, stored, &fetch(&own)).expect("a group")
}

/// The one upsert of an outcome.
fn patch(outcome: &Outcome) -> &ConversationPatch {
    let [BackendEvent::ConversationUpsert(patch)] = outcome.events.as_slice() else {
        panic!("not exactly one upsert: {} events", outcome.events.len());
    };
    patch
}

fn roles(patch: &ConversationPatch) -> Vec<(UserId, MemberRole)> {
    let list = patch.members.as_ref().expect("members");
    assert!(list.full, "a snapshot, never a delta");
    list.upsert
        .iter()
        .map(|member| (member.user.clone(), member.role))
        .collect()
}

/// A `Found` answer is the whole group in ONE patch: members as a snapshot, the features beside them (M4), the title, the
/// description, the timer with the fetch's time as its version, and the row says so.
#[test]
fn a_found_group_is_stated_whole_in_one_patch() {
    let mut data = metadata();
    data.description = Some("Synthetic description".into());
    data.ephemeral = Some(GroupEphemeralSettings {
        expiration: Some(86_400),
        trigger: Some(0),
    });
    let out = outcome(&found(data), &State::default());
    let patch = patch(&out);
    assert_eq!(patch.key, conv());
    assert_eq!(patch.title, Patch::Set("Synthetic group".to_owned()));
    assert_eq!(
        patch.description,
        Patch::Set("Synthetic description".to_owned())
    );
    assert_eq!(
        roles(patch),
        [
            (lid(ADA_LID), MemberRole::Admin),
            (lid(BOB_LID), MemberRole::Member),
            (lid(OUR_LID), MemberRole::Member),
        ]
    );
    assert_eq!(patch.timer, Patch::Set(Duration::from_secs(86_400)));
    assert_eq!(patch.timer_version, Some(AT_SECS));
    assert_eq!(patch.parent, Patch::Clear);
    assert!(
        patch.features.is_some(),
        "the members are never stated without the features"
    );
    assert_eq!(
        out.state,
        State {
            members_stated: true,
            subject_time: Some(1_700_000_100),
            timer: Some(86_400),
            timer_version: Some(AT_SECS),
            rules: Some(Rules::default()),
            fetched_at: Some(AT_MS),
            ..State::default()
        }
    );
}

/// The contract has no owner: a super admin is an `Admin` (D10), and so is an admin; the rest are members.
#[test]
fn a_super_admin_is_an_admin_and_nobody_else_is() {
    let mut data = metadata();
    data.participants = vec![
        who(&format!("{ADA_LID}@lid"), ParticipantType::SuperAdmin, None),
        who(&format!("{BOB_LID}@lid"), ParticipantType::Admin, None),
        who(&format!("{OUR_LID}@lid"), ParticipantType::Member, None),
    ];
    let out = outcome(&found(data), &State::default());
    assert_eq!(
        roles(patch(&out)),
        [
            (lid(ADA_LID), MemberRole::Admin),
            (lid(BOB_LID), MemberRole::Admin),
            (lid(OUR_LID), MemberRole::Member),
        ]
    );
}

/// A person named twice is one member, with the higher role; an entry that is no person is left out.
#[test]
fn a_person_named_twice_is_one_member_with_the_higher_role() {
    let mut data = metadata();
    data.participants = vec![
        who(&format!("{ADA_LID}@lid"), ParticipantType::Member, None),
        who(&format!("{ADA_LID}@lid"), ParticipantType::Admin, None),
        who(&format!("{BOB_LID}@lid"), ParticipantType::Admin, None),
        who(&format!("{BOB_LID}@lid"), ParticipantType::Member, None),
        who("120363000000000099@g.us", ParticipantType::Admin, None),
    ];
    let out = outcome(&found(data), &State::default());
    assert_eq!(
        roles(patch(&out)),
        [
            (lid(ADA_LID), MemberRole::Admin),
            (lid(BOB_LID), MemberRole::Admin),
        ]
    );
}

/// A group in the number's mode lists its members by number.
#[test]
fn a_number_group_lists_its_members_by_number() {
    let mut data = metadata();
    data.participants = vec![
        who(
            &format!("{ADA_PN}@s.whatsapp.net"),
            ParticipantType::Admin,
            None,
        ),
        who(
            &format!("{CY_PN}@s.whatsapp.net"),
            ParticipantType::Member,
            None,
        ),
    ];
    let out = outcome(&found(data), &State::default());
    assert_eq!(
        roles(patch(&out)),
        [
            (pn(ADA_PN), MemberRole::Admin),
            (pn(CY_PN), MemberRole::Member)
        ]
    );
}

/// An announcement group: we cannot send unless one of our ids is an admin, found through whichever form the list names
/// us by (our LID as the id, our number beside another's id, our LID in the `lid` field).
#[test]
fn only_admins_send_to_an_announcement_group_and_our_place_is_found_through_both_forms() {
    let announcement = |participants: Vec<GroupParticipant>| {
        let mut data = metadata();
        data.is_announcement = true;
        data.participants = participants;
        outcome(&found(data), &State::default())
    };
    let cannot_send = |out: &Outcome| {
        patch(out)
            .features
            .as_ref()
            .expect("features")
            .cannot_send
            .clone()
    };

    // A plain member: admins only.
    let out = announcement(vec![
        who(&format!("{ADA_LID}@lid"), ParticipantType::Admin, None),
        who(&format!("{OUR_LID}@lid"), ParticipantType::Member, None),
    ]);
    assert_eq!(cannot_send(&out), Some(CannotSend::AdminsOnly));
    assert_eq!(out.state.rules.map(|rules| rules.own_admin), Some(false));

    // An admin by our LID.
    let out = announcement(vec![who(
        &format!("{OUR_LID}@lid"),
        ParticipantType::Admin,
        None,
    )]);
    assert_eq!(cannot_send(&out), None);
    assert_eq!(out.state.rules.map(|rules| rules.own_admin), Some(true));

    // A super admin by our NUMBER, listed under a LID we do not know of.
    let out = announcement(vec![who(
        "99000000999@lid",
        ParticipantType::SuperAdmin,
        Some(&format!("{OUR_PN}@s.whatsapp.net")),
    )]);
    assert_eq!(cannot_send(&out), None);

    // An admin whose `lid` field is ours.
    let mut other = who(
        &format!("{ADA_PN}@s.whatsapp.net"),
        ParticipantType::Admin,
        None,
    );
    other.lid = Some(jid(&format!("{OUR_LID}@lid")));
    let out = announcement(vec![other]);
    assert_eq!(cannot_send(&out), None);
}

/// An open group is sendable whoever we are; `StillLoading` is never stated (D10).
#[test]
fn an_open_group_is_sendable_and_still_loading_is_never_stated() {
    let out = outcome(&found(metadata()), &State::default());
    let features = patch(&out).features.clone().expect("features");
    assert_eq!(features.cannot_send, None);
    assert!(features.group_info.is_some(), "the rules are stated");
}

/// The rules in the core's terms: locked, members added by admins only, announcements, suspended.
#[test]
fn the_rules_are_stated_as_the_groups_info() {
    let mut data = metadata();
    data.is_locked = true;
    data.member_add_mode = Some(MemberAddMode::AdminAdd);
    data.is_suspended = true;
    let out = outcome(&found(data), &State::default());
    let info = patch(&out)
        .features
        .as_ref()
        .and_then(|features| features.group_info)
        .expect("group info");
    assert_eq!(
        info,
        GroupInfo {
            attributes_admin_only: true,
            members_admin_only: true,
            announcements_only: false,
            terminated: true,
        }
    );

    let mut data = metadata();
    data.member_add_mode = Some(MemberAddMode::AllMemberAdd);
    let out = outcome(&found(data), &State::default());
    let info = patch(&out)
        .features
        .as_ref()
        .and_then(|features| features.group_info)
        .expect("group info");
    assert_eq!(info, GroupInfo::default());
}

/// Fix F4 (PROTOCOL-7): a suspended group is ended for everybody, and the core gates a send on `cannot_send` alone, so
/// the row states it (the nearest reason the contract has) beside `terminated`, whether the row comes from a fetch or from
/// a notice, and over `AdminsOnly`; a group that is not suspended keeps its own reason.
/// Mutant: `cannot_send` not set for a suspended group.
#[test]
fn a_suspended_group_cannot_be_sent_to() {
    let mut data = metadata();
    data.is_suspended = true;
    let out = outcome(&found(data), &State::default());
    let features = patch(&out).features.clone().expect("features");
    assert_eq!(features.cannot_send, Some(CannotSend::NotMember));
    assert!(features.group_info.expect("info").terminated);
    // Over the announcement reason, and from the row alone.
    let row = State {
        rules: Some(Rules {
            suspended: true,
            announcements_only: true,
            ..Rules::default()
        }),
        ..State::default()
    };
    assert_eq!(
        super::features(&conv(), &row).cannot_send,
        Some(CannotSend::NotMember)
    );
    // Not suspended: an open group sends.
    let open = State {
        rules: Some(Rules::default()),
        ..State::default()
    };
    assert_eq!(super::features(&conv(), &open).cannot_send, None);
}

/// `Truncated` has no members and no settings: nothing is stated (never an empty member list), the members the core
/// holds stay, and the row says the group owes a single query.
#[test]
fn a_truncated_group_states_nothing_and_keeps_what_the_core_has() {
    let stored = State {
        members_stated: true,
        subject_time: Some(1_700_000_100),
        rules: Some(Rules {
            announcements_only: true,
            ..Rules::default()
        }),
        ..State::default()
    };
    let result = GroupMetadataResult::Truncated {
        id: jid(GROUP),
        participant_count: 1500,
    };
    let out = outcome(&result, &stored);
    assert!(
        out.events.is_empty(),
        "no member list, not even an empty one"
    );
    assert_eq!(out.conv, conv());
    assert_eq!(
        out.state,
        State {
            truncated: true,
            fetched_at: Some(AT_MS),
            ..stored
        }
    );

    // A group never answered whole stays without a member list: its admin deletes keep waiting (M1).
    let out = outcome(&result, &State::default());
    assert!(!out.state.members_stated);
    assert!(out.state.truncated);
}

/// A whole answer after a truncated one takes the mark off.
#[test]
fn a_whole_answer_ends_the_truncated_mark() {
    let stored = State {
        truncated: true,
        ..State::default()
    };
    let out = outcome(&found(metadata()), &stored);
    assert!(!out.state.truncated);
    assert!(out.state.members_stated);
}

/// An answer with no participants is no member list (the server always lists us at least): nothing replaces the list.
#[test]
fn an_answer_without_participants_is_no_member_list() {
    let mut data = metadata();
    data.participants = Vec::new();
    let stored = State {
        members_stated: true,
        ..State::default()
    };
    let out = outcome(&found(data), &stored);
    assert!(patch(&out).members.is_none());
    assert!(
        out.state.members_stated,
        "what the core was told stays told"
    );
    let mut data = metadata();
    data.participants = Vec::new();
    let out = outcome(&found(data), &State::default());
    assert!(!out.state.members_stated);
}

/// Fix F2 (PROTOCOL-2): a `Found` answer the server marks incomplete (`missing_participant_identification`) is no snapshot.
/// On a group whose list the core never got it states no members at all and `members_stated` stays false (a parked admin
/// delete is not released by it); on a group whose list the core holds it states an upsert-only delta of the people it
/// names, never a removal and never `full`; both ways the row is marked like a `Truncated` one (the single query asks it
/// whole), and the title and the rules of the answer are still stated. A list that does not name us says nothing of our
/// place (the row's own admin bit stays).
/// Mutant: the flag ignored (`incomplete = false`).
#[test]
fn a_list_the_server_marks_incomplete_is_no_snapshot() {
    let mut data = metadata();
    data.has_missing_participant_identification = true;

    // No list stated yet: nothing of the members is stated and nothing is claimed.
    let out = outcome(&found(data.clone()), &State::default());
    let one = patch(&out);
    assert!(one.members.is_none(), "no partial list on no list");
    assert_eq!(one.title, Patch::Set("Synthetic group".into()));
    assert!(one.features.is_some());
    assert!(!out.state.members_stated, "so nothing waiting is released");
    assert!(out.state.truncated, "the single query asks it whole");

    // A list stated: a delta that only adds, and the row keeps saying that the core holds the list.
    let stored = State {
        members_stated: true,
        rules: Some(Rules {
            own_admin: true,
            ..Rules::default()
        }),
        ..State::default()
    };
    let out = outcome(&found(data.clone()), &stored);
    let list = patch(&out).members.clone().expect("a delta");
    assert!(!list.full, "never a snapshot");
    assert!(list.remove.is_empty(), "never a removal");
    assert_eq!(list.upsert.len(), 3);
    assert!(out.state.members_stated && out.state.truncated);
    assert_eq!(
        out.state.rules.map(|rules| rules.own_admin),
        Some(false),
        "the list names us, as a member"
    );

    // The list that does not name us says nothing of our place.
    data.participants
        .retain(|p| !p.jid.to_string().starts_with(OUR_LID));
    let out = outcome(&found(data.clone()), &stored);
    assert_eq!(out.state.rules.map(|rules| rules.own_admin), Some(true));
    // The same list unflagged is the snapshot of old: it names us not, so we are not an admin and the list is whole.
    data.has_missing_participant_identification = false;
    let out = outcome(&found(data), &stored);
    assert!(patch(&out).members.as_ref().is_some_and(|list| list.full));
    assert!(!out.state.truncated);
    assert_eq!(out.state.rules.map(|rules| rules.own_admin), Some(false));
}

/// `Forbidden` and `NotFound` mean we are not in the group: `NotMember`, no rules, and the members stay as they were.
#[test]
fn a_group_we_are_not_in_is_stated_not_a_member_whichever_way_the_server_says_so() {
    let stored = State {
        members_stated: true,
        rules: Some(Rules {
            locked: true,
            ..Rules::default()
        }),
        ..State::default()
    };
    for result in [
        GroupMetadataResult::Forbidden(jid(GROUP)),
        GroupMetadataResult::NotFound(jid(GROUP)),
    ] {
        let out = outcome(&result, &stored);
        let patch = patch(&out);
        assert!(patch.members.is_none(), "the members stay as they were");
        assert_eq!(patch.title, Patch::Unchanged);
        let features = patch.features.as_ref().expect("features");
        assert_eq!(features.cannot_send, Some(CannotSend::NotMember));
        assert!(features.group_info.is_none());
        assert!(out.state.not_member);
        assert!(out.state.members_stated, "it was stated once and stays so");
    }
}

/// A group we were removed from and are in again is a member again.
#[test]
fn a_whole_answer_after_not_a_member_makes_us_a_member_again() {
    let stored = State {
        not_member: true,
        ..State::default()
    };
    let out = outcome(&found(metadata()), &stored);
    assert!(!out.state.not_member);
    let features = patch(&out).features.clone().expect("features");
    assert_eq!(features.cannot_send, None);
}

/// A title is set unless the core was told a NEWER one (M5); a side with no time is never "older" (T6).
#[test]
fn an_older_title_never_replaces_a_newer_one() {
    let stored = |time: Option<u64>| State {
        subject_time: time,
        ..State::default()
    };
    let title_of = |data: GroupMetadata, stored: &State| {
        let out = outcome(&found(data), stored);
        (patch(&out).title.clone(), out.state.subject_time)
    };
    let dated = |time: Option<u64>| {
        let mut data = metadata();
        data.subject_time = time;
        data
    };
    let set = Patch::Set("Synthetic group".to_owned());

    // Older: not set, the stored time stays.
    assert_eq!(
        title_of(dated(Some(1_700_000_000)), &stored(Some(1_700_000_500))),
        (Patch::Unchanged, Some(1_700_000_500))
    );
    // Newer, equal, and nothing stored: set, the time moves up.
    assert_eq!(
        title_of(dated(Some(1_700_000_900)), &stored(Some(1_700_000_500))),
        (set.clone(), Some(1_700_000_900))
    );
    assert_eq!(
        title_of(dated(Some(1_700_000_500)), &stored(Some(1_700_000_500))),
        (set.clone(), Some(1_700_000_500))
    );
    assert_eq!(
        title_of(dated(Some(1_700_000_500)), &stored(None)),
        (set.clone(), Some(1_700_000_500))
    );
    // No time in the answer: the title is the server's current one, the stored time stays.
    assert_eq!(
        title_of(dated(None), &stored(Some(1_700_000_500))),
        (set.clone(), Some(1_700_000_500))
    );
    assert_eq!(title_of(dated(None), &stored(None)), (set, None));
}

/// WhatsApp has no unnamed group: an empty or missing subject changes nothing.
#[test]
fn a_missing_or_empty_subject_changes_no_title() {
    for subject in [None, Some(String::new()), Some("  ".to_owned())] {
        let mut data = metadata();
        data.subject = subject;
        let out = outcome(&found(data), &State::default());
        assert_eq!(patch(&out).title, Patch::Unchanged);
        assert_eq!(out.state.subject_time, None, "no title, no time kept");
    }
}

/// The title and the description are cut, the description's absence clears it (the answer is the whole group).
#[test]
fn texts_are_cut_and_an_absent_description_is_none() {
    let mut data = metadata();
    data.subject = Some("t".repeat(TITLE_CAP + 10));
    data.description = Some("d".repeat(DESCRIPTION_CAP + 10));
    let out = outcome(&found(data), &State::default());
    let stated = patch(&out);
    assert_eq!(stated.title, Patch::Set("t".repeat(TITLE_CAP)));
    assert_eq!(stated.description, Patch::Set("d".repeat(DESCRIPTION_CAP)));

    let out = outcome(&found(metadata()), &State::default());
    assert_eq!(patch(&out).description, Patch::Clear);
}

/// No `<ephemeral>` is no timer, and it is still stated with the fetch's version (T7): without one a late older notice
/// would win in the core.
#[test]
fn a_group_without_a_timer_clears_it_with_the_fetchs_version() {
    for settings in [
        None,
        Some(GroupEphemeralSettings {
            expiration: Some(0),
            trigger: None,
        }),
        Some(GroupEphemeralSettings {
            expiration: None,
            trigger: Some(1),
        }),
    ] {
        let mut data = metadata();
        data.ephemeral = settings;
        let out = outcome(&found(data), &State::default());
        let patch = patch(&out);
        assert_eq!(patch.timer, Patch::Clear);
        assert_eq!(patch.timer_version, Some(AT_SECS));
        assert_eq!(out.state.timer, Some(0));
    }
}

/// A subgroup names its community; anything else clears the parent.
#[test]
fn a_subgroup_names_its_community() {
    let mut data = metadata();
    data.parent_group_jid = Some(jid(PARENT));
    let out = outcome(&found(data), &State::default());
    let community = group_conv(&jid(PARENT)).expect("group");
    assert_eq!(patch(&out).parent, Patch::Set(community.clone()));
    // The row names the parent, as told (the zone may take it back with `withhold_parent`).
    assert_eq!(out.state.parent, Some(community.clone()));
    assert!(out.state.parent_stated);

    // Q3: the core does not know the community yet: the patch leaves it out and the row says it waits.
    let mut withheld = out.clone();
    withhold_parent(&mut withheld);
    assert_eq!(patch(&withheld).parent, Patch::Unchanged);
    assert_eq!(withheld.state.parent, Some(community));
    assert!(!withheld.state.parent_stated);
    assert_eq!(
        patch(&withheld).title,
        patch(&out).title,
        "nothing but the parent is taken out"
    );

    let mut data = metadata();
    data.is_parent_group = true;
    let out = outcome(&found(data), &State::default());
    assert_eq!(patch(&out).parent, Patch::Clear);
    assert_eq!(out.state.parent, None);
    // A group with no parent has nothing to withhold.
    let mut whole = out.clone();
    withhold_parent(&mut whole);
    assert_eq!(whole, out);
}

/// An entry about something that is no group states nothing.
#[test]
fn an_entry_that_is_no_group_states_nothing() {
    let own = own();
    let person = format!("{ADA_PN}@s.whatsapp.net");
    for result in [
        GroupMetadataResult::Forbidden(jid(&person)),
        GroupMetadataResult::NotFound(jid(&person)),
        GroupMetadataResult::Truncated {
            id: jid(&person),
            participant_count: 3,
        },
    ] {
        assert!(map(&result, &State::default(), &fetch(&own)).is_none());
        assert!(conv_of(&result).is_none());
    }
}

/// The group's features are made from the row: the same row gives the same features whenever they are asked for again
/// (a new version, a `Rebuild`), and a group we are not in has no rules.
#[test]
fn the_features_are_made_from_the_row() {
    let row = State {
        rules: Some(Rules {
            announcements_only: true,
            locked: true,
            ..Rules::default()
        }),
        ..State::default()
    };
    let made = super::features(&conv(), &row);
    assert_eq!(made.cannot_send, Some(CannotSend::AdminsOnly));
    assert_eq!(
        made.group_info,
        Some(GroupInfo {
            attributes_admin_only: true,
            announcements_only: true,
            ..GroupInfo::default()
        })
    );

    // Never answered: the chat's own features, no rules claimed.
    let never = super::features(&conv(), &State::default());
    assert_eq!(never, crate::features::of(&conv(), true));

    // Not a member wins over everything.
    let gone = super::features(
        &conv(),
        &State {
            not_member: true,
            ..row
        },
    );
    assert_eq!(gone.cannot_send, Some(CannotSend::NotMember));
    assert!(gone.group_info.is_none());
}

/// The stored bits give the rules back, whichever are on.
#[test]
fn the_rules_survive_their_stored_form() {
    for bits in 0..32_i64 {
        let rules = Rules::from_bits(bits);
        assert_eq!(rules.bits(), bits);
    }
    assert_eq!(Rules::from_bits(1 << 20), Rules::default());
}
