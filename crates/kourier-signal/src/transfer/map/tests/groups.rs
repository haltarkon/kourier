//! Step S4b: groups. The group recipient and its snapshot (memberships, members with roles,
//! the timer in seconds), the chat frame's timer in milliseconds, group items by any member,
//! the group changes as the LIVE decoder makes them (an entry without its actor is skipped,
//! folding before the cap, bidi isolates, the timer in ms) and group calls. Synthetic data.

use kourier_model::{Member, MemberRole};

use super::*;
use crate::archive::writer::{
    CallSpec, GroupCallSpec, GroupChangeSpec, GroupSpec, ItemSpec, NoticeSpec, Party, QuoteSpec,
    ReactionSpec, SnapshotSpec, Who,
};
use crate::transfer::stage::{GroupMembership, StagedGroup};

const T: u64 = 1_700_000_000_000;

fn key(i: u8) -> Vec<u8> {
    vec![i; 32]
}

fn conv(i: u8) -> ConvKey {
    crate::groups::GroupKeys::from_slice(&key(i))
        .unwrap()
        .conv_key()
}

fn pni_user(uuid: Uuid) -> UserId {
    UserId::parse(&format!("pni:{}", uuid.hyphenated())).unwrap()
}

/// A history of three people and the groups `groups`, one chat per group (its items
/// `texts`), with our ids set.
fn history(groups: Vec<GroupSpec>, chats: Vec<ChatSpec>) -> HistorySpec {
    HistorySpec {
        contacts: vec![person(0x21), person(0x22), person(0x23)],
        groups,
        chats,
        own_aci: Some(own_aci()),
        own_pni: Some(own_pni()),
        ..HistorySpec::default()
    }
}

fn group(i: u8, snapshot: Option<SnapshotSpec>) -> GroupSpec {
    GroupSpec {
        master_key: key(i),
        snapshot,
        ..GroupSpec::default()
    }
}

fn snapshot(members: &[(Party, bool)]) -> SnapshotSpec {
    SnapshotSpec {
        title: Some("A group".to_owned()),
        members: members.to_vec(),
        ..SnapshotSpec::default()
    }
}

fn in_group(index: usize, texts: Vec<TextSpec>) -> ChatSpec {
    ChatSpec {
        with: ChatWith::Group(index),
        texts,
        ..ChatSpec::default()
    }
}

/// An incoming text by contact `author` of the history.
fn by(author: usize, body: &str, sent_at: u64) -> TextSpec {
    TextSpec {
        author: Some(Who::Contact(author)),
        ..incoming(body, sent_at, true)
    }
}

fn change(entries: Vec<GroupChangeSpec>, sent_at: u64) -> TextSpec {
    TextSpec {
        // The item's author is us: what Android writes for a change whose editor it did
        // not know. It is never the actor.
        outgoing: true,
        kind: ItemSpec::Notice(NoticeSpec::GroupChange(entries)),
        sent_at,
        ..TextSpec::default()
    }
}

fn staged_group<'a>(staged: &'a [Staged], c: &ConvKey) -> Option<&'a StagedGroup> {
    chats(staged)
        .into_iter()
        .find(|(_, k, _)| *k == c)
        .and_then(|(_, _, chat)| chat.group())
}

fn rows_of<'a>(staged: &'a [Staged], c: &ConvKey) -> Vec<&'a IncomingMessage> {
    messages(staged)
        .into_iter()
        .filter(|m| &m.conv == c)
        .collect()
}

#[test]
fn our_membership_comes_from_the_snapshot_by_our_aci_or_our_pni() {
    let invited_by_pni = SnapshotSpec {
        pending: vec![Party::OwnPni],
        ..snapshot(&[(Party::Contact(0), true)])
    };
    let invited_by_aci = SnapshotSpec {
        pending: vec![Party::Own],
        ..snapshot(&[(Party::Contact(0), true)])
    };
    let requesting = SnapshotSpec {
        requesting: vec![Party::Own],
        ..SnapshotSpec::default()
    };
    let spec = history(
        vec![
            group(
                1,
                Some(snapshot(&[(Party::Own, false), (Party::Contact(0), true)])),
            ),
            group(2, Some(invited_by_pni)),
            group(3, Some(invited_by_aci)),
            group(4, Some(requesting)),
            group(5, Some(snapshot(&[(Party::Contact(0), true)]))),
            group(6, None),
        ],
        (0..6)
            .map(|i| in_group(i, vec![by(0, "hello", T + i as u64)]))
            .collect(),
    );
    let (staged, end) = map_spec(&spec);
    let want = [
        (1, GroupMembership::Member),
        (2, GroupMembership::Invited),
        (3, GroupMembership::Invited),
        (4, GroupMembership::Requesting),
        (5, GroupMembership::Left),
        (6, GroupMembership::Unknown),
    ];
    for (i, membership) in want {
        let group = staged_group(&staged, &conv(i)).expect("the group is staged");
        assert_eq!(group.membership, membership, "group {i}");
    }
    // An empty snapshot says nothing of the members: no list, which would empty one.
    assert_eq!(staged_group(&staged, &conv(6)).unwrap().members, None);
    assert_eq!(staged_group(&staged, &conv(6)).unwrap().title, None);
    assert!(end.counts.is_empty(), "{:?}", end.counts);
}

#[test]
fn a_group_we_are_invited_to_is_staged_and_shown_without_any_row() {
    let spec = history(
        vec![group(
            1,
            Some(SnapshotSpec {
                pending: vec![Party::OwnPni],
                ..snapshot(&[(Party::Contact(0), true)])
            }),
        )],
        vec![in_group(
            0,
            // Its only item, the invitation, is no notice of the contract.
            vec![change(
                vec![GroupChangeSpec::SelfInvited {
                    inviter: Party::Contact(0),
                }],
                T,
            )],
        )],
    );
    let (staged, end) = map_spec(&spec);
    let (_, _, chat) = chats(&staged)
        .into_iter()
        .find(|(_, k, _)| **k == conv(1))
        .expect("staged at its chat frame");
    assert!(chat.invited());
    assert!(rows_of(&staged, &conv(1)).is_empty());
    assert_eq!(count(&end, reason::NOTICE_NOT_IN_CONTRACT), 1);
    // A group we left with nothing loud is not staged.
    let spec = history(
        vec![group(2, Some(snapshot(&[(Party::Contact(0), true)])))],
        vec![in_group(0, Vec::new())],
    );
    let (staged, _) = map_spec(&spec);
    assert!(chats(&staged).is_empty());
}

#[test]
fn members_invited_and_requesting_people_keep_their_roles_and_are_staged_with_names() {
    let stranger = Uuid::from_bytes([0x5a; 16]);
    let invitee = Uuid::from_bytes([0x5b; 16]);
    let spec = history(
        vec![group(
            1,
            Some(SnapshotSpec {
                title: Some("Hikers".to_owned()),
                timer_secs: Some(3_600),
                members: vec![
                    (Party::Own, false),
                    (Party::Contact(0), true),
                    (Party::Contact(1), false),
                    (Party::Bad, false),
                ],
                pending: vec![Party::Pni(invitee), Party::Aci(stranger)],
                requesting: vec![Party::Contact(2)],
                invite_link_password: b"MARKER-PASSWORD".to_vec(),
            }),
        )],
        vec![in_group(0, vec![by(0, "hello", T)])],
    );
    let (staged, end) = map_spec(&spec);
    let group = staged_group(&staged, &conv(1)).unwrap();
    assert_eq!(group.title.as_deref(), Some("Hikers"));
    let member = |user: UserId, role| Member { user, role };
    assert_eq!(
        group.members.as_deref().unwrap(),
        [
            member(own_user(), MemberRole::Member),
            member(user(0x21), MemberRole::Admin),
            member(user(0x22), MemberRole::Member),
            member(pni_user(invitee), MemberRole::Pending),
            member(
                UserId::parse(&format!("aci:{}", stranger.hyphenated())).unwrap(),
                MemberRole::Pending
            ),
            member(user(0x23), MemberRole::Requesting),
        ]
    );
    assert_eq!(count(&end, reason::MEMBER_DROPPED), 1);
    // Every member with a contact frame is staged, the requesting one too.
    let people: Vec<&UserId> = contacts(&staged).into_iter().map(|c| &c.user).collect();
    for i in [0x21, 0x22, 0x23] {
        assert!(people.contains(&&user(i)), "{i:x}");
    }
    // The invite link's password never reaches the stage.
    let json = serde_json::to_vec(&chats(&staged)[0].2).unwrap();
    assert!(
        !json
            .windows(b"MARKER-PASSWORD".len())
            .any(|w| w == b"MARKER-PASSWORD")
    );
    // The master key never reaches a `Debug`.
    let debug = format!("{:?}", chats(&staged)[0].2);
    assert!(
        !debug.contains("66, 66") && !debug.contains("[1, 1, 1"),
        "{debug}"
    );
    assert!(!format!("{group:?}").contains("Hikers"));
}

#[test]
fn the_chat_frames_timer_in_milliseconds_wins_over_the_snapshots_seconds() {
    let with_timer = |secs| {
        Some(SnapshotSpec {
            timer_secs: Some(secs),
            ..snapshot(&[(Party::Own, false)])
        })
    };
    let spec = history(
        vec![group(1, with_timer(3_600)), group(2, with_timer(600))],
        vec![
            ChatSpec {
                expiration_timer_ms: Some(86_400_000),
                ..in_group(0, vec![by(0, "a", T)])
            },
            in_group(1, vec![by(0, "b", T + 1)]),
        ],
    );
    let (staged, _) = map_spec(&spec);
    let timer = |c: &ConvKey| {
        chats(&staged)
            .into_iter()
            .find(|(_, k, _)| *k == c)
            .unwrap()
            .2
            .frame
            .timer_secs
    };
    assert_eq!(timer(&conv(1)), Some(86_400), "the chat frame's, ms to s");
    assert_eq!(timer(&conv(2)), Some(600), "the snapshot's, in seconds");
}

#[test]
fn group_items_name_any_member_and_our_own_take_the_state_over_every_member() {
    let spec = history(
        vec![group(
            1,
            Some(snapshot(&[
                (Party::Own, false),
                (Party::Contact(0), false),
                (Party::Contact(1), false),
            ])),
        )],
        vec![in_group(
            0,
            vec![
                by(0, "from Ann", T),
                TextSpec {
                    quote: Some(QuoteSpec {
                        target: Some(T),
                        author: Some(Who::Contact(0)),
                        text: "from Ann".to_owned(),
                        ..QuoteSpec::default()
                    }),
                    reactions: vec![ReactionSpec {
                        by: Who::Contact(2),
                        emoji: "\u{1f44d}".to_owned(),
                        sent_at: T + 5,
                        sort_order: 1,
                    }],
                    ..by(1, "from Bo, quoting Ann", T + 1)
                },
                TextSpec {
                    statuses_by: vec![
                        (Who::Contact(0), SendStatusSpec::Read),
                        (Who::Contact(1), SendStatusSpec::Delivered),
                    ],
                    ..outgoing("ours", T + 2, &[])
                },
            ],
        )],
    );
    let (staged, end) = map_spec(&spec);
    let rows = rows_of(&staged, &conv(1));
    assert_eq!(rows.len(), 3, "{:?}", end.counts);
    assert!(matches!(&rows[0].origin, Origin::Remote { author, .. } if *author == user(0x21)));
    assert!(matches!(&rows[1].origin, Origin::Remote { author, .. } if *author == user(0x22)));
    let quote = rows[1].reply_to.as_ref().unwrap();
    assert_eq!(quote.target.author, TargetAuthor::User(user(0x21)));
    assert_eq!(rows[1].reactions.len(), 1);
    assert_eq!(rows[1].reactions[0].by, Some(user(0x23)));
    assert_eq!(rows[2].initial_out_state, Some(OutgoingState::Delivered));
    // The reactor, not in the snapshot, is staged too (the row names them).
    let people: Vec<&UserId> = contacts(&staged).into_iter().map(|c| &c.user).collect();
    assert!(people.contains(&&user(0x23)));
}

#[test]
fn a_group_change_becomes_the_live_paths_notices_and_skips_an_entry_without_its_actor() {
    let spec = history(
        vec![group(1, Some(snapshot(&[(Party::Own, false)])))],
        vec![in_group(
            0,
            vec![change(
                vec![
                    GroupChangeSpec::Created {
                        by: Party::Contact(0),
                    },
                    // Our PNI is us: `by: None`.
                    GroupChangeSpec::Name {
                        by: Party::OwnPni,
                        name: Some("\u{2068}Café\u{2069}".to_owned()),
                    },
                    GroupChangeSpec::Name {
                        by: Party::Contact(1),
                        name: None,
                    },
                    // Absent, empty, unparsable actors: skipped, never "You".
                    GroupChangeSpec::Avatar { by: Party::Absent },
                    GroupChangeSpec::Description { by: Party::Empty },
                    GroupChangeSpec::Timer {
                        by: Party::Bad,
                        expires_in_ms: 60_000,
                    },
                    GroupChangeSpec::Timer {
                        by: Party::Contact(0),
                        expires_in_ms: 3_600_000,
                    },
                    GroupChangeSpec::MemberLeft {
                        who: Party::Contact(2),
                    },
                    GroupChangeSpec::MemberRemoved {
                        by: Party::Contact(0),
                        removed: Party::Contact(1),
                    },
                    // A join of somebody else names them; ours is `Joined`.
                    GroupChangeSpec::JoinedByLink {
                        who: Party::Contact(2),
                    },
                    GroupChangeSpec::InvitationAccepted { who: Party::Own },
                    // "Ann added you".
                    GroupChangeSpec::MemberAdded {
                        by: Party::Contact(0),
                        who: Party::Own,
                    },
                    GroupChangeSpec::JoinRequestApproval {
                        by: Party::Contact(0),
                        requestor: Party::Contact(2),
                        approved: false,
                    },
                    GroupChangeSpec::Generic {
                        by: Party::Contact(0),
                    },
                    GroupChangeSpec::AdminStatus {
                        by: Party::Contact(0),
                        member: Party::Contact(1),
                    },
                    GroupChangeSpec::Unset,
                ],
                T,
            )],
        )],
    );
    let (staged, end) = map_spec(&spec);
    let rows = rows_of(&staged, &conv(1));
    let events: Vec<&SystemEvent> = rows
        .iter()
        .map(|m| match &m.body {
            Body::System(event) => event,
            _ => panic!("a notice"),
        })
        .collect();
    let (ann, bo, cy) = (user(0x21), user(0x22), user(0x23));
    assert_eq!(
        events,
        [
            &SystemEvent::GroupCreated {
                by: Some(ann.clone())
            },
            &SystemEvent::TitleChanged {
                by: None,
                title: "Café".to_owned()
            },
            &SystemEvent::TitleChanged {
                by: Some(bo.clone()),
                title: String::new()
            },
            &SystemEvent::TimerChanged {
                by: Some(ann.clone()),
                timer: Some(Duration::from_secs(3_600))
            },
            &SystemEvent::MemberLeft { user: cy.clone() },
            &SystemEvent::MembersRemoved {
                by: Some(ann.clone()),
                users: vec![bo.clone()]
            },
            &SystemEvent::MembersAdded {
                by: Some(cy.clone()),
                users: vec![cy.clone()]
            },
            &SystemEvent::Joined,
            &SystemEvent::MembersAdded {
                by: Some(ann.clone()),
                users: vec![own_user()]
            },
        ]
    );
    assert_eq!(count(&end, reason::GROUP_CHANGE_NO_ACTOR), 3);
    assert_eq!(count(&end, reason::NOTICE_NOT_IN_CONTRACT), 4);
    // Ids of their own, one per notice, and their place in the item as their order.
    let frame = rows[0].local.split(':').nth(1).unwrap().to_owned();
    for (k, row) in rows.iter().enumerate() {
        assert_eq!(&*row.local, format!("h:{frame}:{k}"));
        assert_eq!(row.sent_at, T as i64 + k as i64);
        assert!(row.read);
    }
    // The actor is the origin, never the item's author (us here); ours is our own device.
    assert!(matches!(&rows[0].origin, Origin::Remote { author, .. } if *author == ann));
    assert!(rows[1].origin.is_own());
    assert!(rows[7].origin.is_own());
    // A timer-change notice keeps no timer.
    assert_eq!(rows[3].expiry, None);
}

#[test]
fn members_added_by_one_actor_are_folded_into_one_notice_before_the_cap() {
    let many: Vec<GroupChangeSpec> = (0..300u32)
        .map(|i| {
            let mut bytes = [0x77; 16];
            bytes[..4].copy_from_slice(&i.to_be_bytes());
            GroupChangeSpec::MemberAdded {
                by: Party::Contact(0),
                who: Party::Aci(Uuid::from_bytes(bytes)),
            }
        })
        .chain((0..3).map(|_| GroupChangeSpec::MemberRemoved {
            by: Party::Contact(1),
            removed: Party::Contact(2),
        }))
        .collect();
    // And an item of 70 notices that cannot fold: 64 stay.
    let names: Vec<GroupChangeSpec> = (0..70)
        .map(|i| GroupChangeSpec::Name {
            by: Party::Contact(0),
            name: Some(format!("name {i}")),
        })
        .collect();
    let spec = history(
        vec![group(1, Some(snapshot(&[(Party::Own, false)])))],
        vec![in_group(0, vec![change(many, T), change(names, T + 1_000)])],
    );
    let (staged, end) = map_spec(&spec);
    let rows = rows_of(&staged, &conv(1));
    assert_eq!(rows.len(), 2 + 64);
    match &rows[0].body {
        Body::System(SystemEvent::MembersAdded { by, users }) => {
            assert_eq!(by, &Some(user(0x21)));
            assert_eq!(users.len(), 300);
        }
        other => panic!("{other:?}"),
    }
    match &rows[1].body {
        Body::System(SystemEvent::MembersRemoved { users, .. }) => {
            assert_eq!(users, &[user(0x23)], "a person once");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(&end, reason::GROUP_NOTICES_CUT), 6);
}

#[test]
fn a_group_call_is_a_line_of_its_group_and_a_11_call_there_is_not() {
    let spec = history(
        vec![group(
            1,
            Some(snapshot(&[(Party::Own, false), (Party::Contact(0), false)])),
        )],
        vec![in_group(
            0,
            vec![
                TextSpec {
                    kind: ItemSpec::Call(CallSpec::Group {
                        call_id: Some(77),
                        state: GroupCallSpec::Missed,
                        ringer: Some(Who::Contact(0)),
                        started_by: None,
                        read: false,
                        started_at: T,
                    }),
                    sent_at: T,
                    ..TextSpec::default()
                },
                TextSpec {
                    kind: ItemSpec::Call(CallSpec::Group {
                        call_id: Some(78),
                        state: GroupCallSpec::Generic,
                        ringer: None,
                        // Started by us: no `by`.
                        started_by: Some(Who::Own),
                        read: true,
                        started_at: T + 1,
                    }),
                    sent_at: T + 1,
                    ..TextSpec::default()
                },
                TextSpec {
                    kind: ItemSpec::Call(CallSpec::Individual {
                        call_id: Some(79),
                        video: false,
                        outgoing: false,
                        state: crate::archive::writer::IndividualCallSpec::Missed,
                        read: false,
                        started_at: T + 2,
                    }),
                    sent_at: T + 2,
                    ..TextSpec::default()
                },
            ],
        )],
    );
    let (staged, end) = map_spec(&spec);
    let rows = rows_of(&staged, &conv(1));
    assert_eq!(rows.len(), 2);
    let line = |m: &IncomingMessage| match &m.body {
        Body::System(SystemEvent::Call(line)) => line.clone(),
        _ => panic!("a call line"),
    };
    assert_eq!(
        &*rows[0].local, "gcall:77",
        "the decimal callId, as the live sync names it"
    );
    assert!(!rows[0].read, "a missed call the phone had not seen");
    assert_eq!(line(rows[0]).state, CallState::Missed);
    assert_eq!(line(rows[0]).by, Some(user(0x21)));
    assert_eq!(line(rows[1]).by, None);
    assert_eq!(count(&end, reason::CALL_NOT_IN_ITS_CHAT), 1);
}

#[test]
fn a_group_with_a_bad_key_is_skipped_and_one_named_twice_is_one_conversation() {
    let spec = history(
        vec![
            GroupSpec {
                master_key: vec![1; 31],
                ..GroupSpec::default()
            },
            group(2, Some(snapshot(&[(Party::Own, false)]))),
            group(
                2,
                Some(SnapshotSpec {
                    title: Some("Other".to_owned()),
                    ..snapshot(&[(Party::Own, false)])
                }),
            ),
        ],
        vec![
            in_group(0, vec![by(0, "nowhere", T)]),
            in_group(1, vec![by(0, "first", T + 1)]),
            in_group(2, vec![by(0, "second", T + 2)]),
        ],
    );
    let (staged, end) = map_spec(&spec);
    assert_eq!(count(&end, reason::GROUP_KEY_INVALID), 1);
    assert_eq!(count(&end, reason::DUPLICATE_GROUP), 1);
    assert_eq!(
        rows_of(&staged, &conv(2)).len(),
        2,
        "both chats' rows in one"
    );
    assert_eq!(
        staged_group(&staged, &conv(2)).unwrap().title.as_deref(),
        Some("A group"),
        "the first frame stands"
    );
}

/// Our own leave is a notice of our own device, like every other notice of ours: its origin
/// is not a remote author who happens to be us, and its timer (the kind follows the origin)
/// is an own notice's, started when the phone started it.
#[test]
fn our_own_leave_is_a_notice_of_our_own_device_and_gets_an_own_timer() {
    let spec = history(
        vec![group(1, Some(snapshot(&[(Party::Contact(0), false)])))],
        vec![in_group(
            0,
            vec![TextSpec {
                expires_in_ms: Some(3_600_000),
                expire_start: Some(T + 5),
                ..change(
                    vec![
                        GroupChangeSpec::MemberLeft { who: Party::Own },
                        GroupChangeSpec::MemberLeft {
                            who: Party::Contact(1),
                        },
                    ],
                    T,
                )
            }],
        )],
    );
    let (staged, _) = map_spec(&spec);
    let rows = rows_of(&staged, &conv(1));
    assert_eq!(rows.len(), 2);
    let left = |row: &IncomingMessage| match &row.body {
        Body::System(SystemEvent::MemberLeft { user }) => user.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(left(rows[0]), own_user());
    assert!(
        rows[0].origin.is_own(),
        "our leave: our device, {:?}",
        rows[0].origin
    );
    assert_eq!(left(rows[1]), user(0x22));
    assert!(matches!(&rows[1].origin, Origin::Remote { author, .. } if *author == user(0x22)));
    let kind = |row: &IncomingMessage| row.expiry.as_ref().map(|e| e.kind);
    assert_eq!(kind(rows[0]), Some(DisappearKind::AfterSend));
    assert_eq!(kind(rows[1]), Some(DisappearKind::AfterRead));
}

/// A person invited by PNI whose contact frame leads with an ACI (and also holds the PNI)
/// is staged with the group, so that the invitee has a name: the group's list names the PNI.
#[test]
fn a_person_invited_by_pni_is_staged_with_the_group_when_their_contact_frame_has_an_aci() {
    let invitee_pni = Uuid::from_bytes([0x6b; 16]);
    let mut spec = history(
        vec![group(
            1,
            Some(SnapshotSpec {
                pending: vec![Party::Pni(invitee_pni)],
                ..snapshot(&[(Party::Own, false), (Party::Contact(1), false)])
            }),
        )],
        vec![in_group(0, vec![by(1, "hello", T)])],
    );
    // Contact 0 is known by an ACI and by this PNI.
    spec.contacts[0].pni = Some(invitee_pni);
    let (staged, _) = map_spec(&spec);
    let group = staged_group(&staged, &conv(1)).unwrap();
    assert!(
        group
            .members
            .as_deref()
            .unwrap()
            .iter()
            .any(|m| m.user == pni_user(invitee_pni) && m.role == MemberRole::Pending)
    );
    let people: Vec<&UserId> = contacts(&staged).into_iter().map(|c| &c.user).collect();
    assert!(
        people.contains(&&user(0x21)),
        "the invitee has a contact frame by ACI and is staged with the group: {people:?}"
    );
    // A contact frame that holds no PNI of this invitee does not stage anybody.
    let mut other = spec.clone();
    other.contacts[0].pni = Some(Uuid::from_bytes([0x6c; 16]));
    let (staged, _) = map_spec(&other);
    let people: Vec<&UserId> = contacts(&staged).into_iter().map(|c| &c.user).collect();
    assert!(!people.contains(&&user(0x21)), "{people:?}");
}

/// Two contact frames of different people that carry the same PNI (no real archive has them):
/// the first person owns the PNI (`or_insert`). When the second falls over the memory cap and is
/// taken back, the PNI still leads to the first, and only the second person is gone; a PNI that
/// is the taken-back person's own goes with them.
#[test]
fn a_person_taken_back_at_the_cap_leaves_the_pni_of_another_person_alone() {
    use backup::recipient::Destination;
    let shared = Uuid::from_bytes([0x6b; 16]);
    let own_pni = Uuid::from_bytes([0x6c; 16]);
    let frame = |id: u64, i: u8, pni: Uuid| {
        Frame::Recipient(backup::Recipient {
            id,
            destination: Some(Destination::Contact(backup::Contact {
                aci: Some(aci(i).as_bytes().to_vec()),
                pni: Some(pni.as_bytes().to_vec()),
                profile_given_name: Some(format!("Person {i}")),
                profile_sharing: true,
                ..Default::default()
            })),
        })
    };
    let mut mapper = mapper_with(MapLimits::default());
    let mut out = Vec::new();
    mapper.frame(frame(2, 0x21, shared), &mut out);
    assert_eq!(mapper.person_of(&pni_user(shared)), Some(0));
    // From here on not one more frame fits.
    mapper.limits.max_recipient_bytes = mapper.recipient_bytes;
    mapper.frame(frame(3, 0x22, shared), &mut out);
    mapper.frame(frame(4, 0x23, own_pni), &mut out);
    assert_eq!(mapper.people.len(), 1, "the other two were taken back");
    assert_eq!(mapper.person_of(&user(0x22)), None);
    assert_eq!(mapper.person_of(&user(0x23)), None);
    assert_eq!(
        mapper.person_of(&pni_user(shared)),
        Some(0),
        "the first person keeps the PNI the second one also named"
    );
    assert_eq!(
        mapper.person_of(&pni_user(own_pni)),
        None,
        "a PNI of the taken-back person goes with them"
    );
    assert_eq!(count(&mapper.finish(), reason::RECIPIENT_OVER_CAP), 2);
}

/// Joins and promotions make the notices the LIVE decoder makes (`groups/tests.rs`
/// `accepted_and_promoted_invitations_make_the_notices_the_import_makes` holds the other
/// half): our own join, join by a link and accepted invitation are all `Joined`; somebody
/// else's join or accepted invitation names them as `by`; a member whom an admin added
/// (Android: `hadOpenInvitation` or not) was added by that admin.
#[test]
fn joins_and_promotions_read_as_the_live_decoder_reads_them() {
    let spec = history(
        vec![group(1, Some(snapshot(&[(Party::Own, false)])))],
        vec![in_group(
            0,
            vec![change(
                vec![
                    GroupChangeSpec::MemberJoined { who: Party::Own },
                    GroupChangeSpec::JoinedByLink { who: Party::Own },
                    GroupChangeSpec::InvitationAccepted { who: Party::Own },
                    GroupChangeSpec::InvitationAccepted {
                        who: Party::Contact(2),
                    },
                    GroupChangeSpec::MemberJoined {
                        who: Party::Contact(1),
                    },
                    GroupChangeSpec::MemberAdded {
                        by: Party::Contact(0),
                        who: Party::Contact(1),
                    },
                ],
                T,
            )],
        )],
    );
    let (staged, _) = map_spec(&spec);
    let events: Vec<SystemEvent> = rows_of(&staged, &conv(1))
        .iter()
        .map(|m| match &m.body {
            Body::System(event) => event.clone(),
            _ => panic!("a notice"),
        })
        .collect();
    let (ann, bo, cy) = (user(0x21), user(0x22), user(0x23));
    assert_eq!(
        events,
        [
            SystemEvent::Joined,
            SystemEvent::Joined,
            SystemEvent::Joined,
            SystemEvent::MembersAdded {
                by: Some(cy.clone()),
                users: vec![cy]
            },
            SystemEvent::MembersAdded {
                by: Some(bo.clone()),
                users: vec![bo.clone()]
            },
            SystemEvent::MembersAdded {
                by: Some(ann),
                users: vec![bo]
            },
        ]
    );
}
