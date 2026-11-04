//! W3b, chunks C6b and C6c: a group's notification as a notice, and the placeholder of a message that
//! cannot be read. Numbers are the fictitious range; nothing names a person.

use std::sync::Arc;

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_model::{Body, ConvKey, Origin as Sender, Patch, Placeholder, SystemEvent, UserId};
use wacore::chrono::{DateTime, Utc};
use wacore::types::events::UnavailableType;
use wacore::types::message::{MessageInfo, MessageSource};

use super::*;

const NOW: u64 = 1_800_000_000_000;
const GROUP: &str = "120363000000000001@g.us";
const OUR_LID: &str = "99900100123";
const OUR_PN: &str = "15550100123";
const ADA_LID: &str = "99000000002";
const ADA_PN: &str = "31000000002";
const BOB_LID: &str = "99000000003";

fn own() -> OwnIds {
    OwnIds {
        lid: Some(UserId::from_parts("lid", OUR_LID).unwrap()),
        pn: Some(UserId::from_parts("pn", OUR_PN).unwrap()),
    }
}

fn lid(user: &str) -> UserId {
    UserId::from_parts("lid", user).unwrap()
}

fn jid(text: &str) -> Jid {
    text.parse().expect("jid")
}

fn time(secs: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs, 0).expect("time")
}

fn member(text: &str) -> GroupParticipantInfo {
    GroupParticipantInfo {
        jid: jid(text),
        phone_number: None,
        display_name: None,
        r#type: None,
        lid: None,
        username: None,
        join_time: None,
        group_history_sent_state: None,
    }
}

/// A notification of the group, caused by `actor` (a JID text), with one action.
fn update(actor: Option<&str>, action: Action) -> GroupUpdate {
    let mut update = GroupUpdate::builder()
        .group_jid(jid(GROUP))
        .notification_id("3178000001".to_owned())
        .action_index(0)
        .timestamp(time(1_700_000_100))
        .is_lid_addressing_mode(true)
        .action(Box::new(action))
        .build();
    update.participant = actor.map(jid);
    update
}

fn noticed(update: &GroupUpdate) -> Noticed {
    group_notice(update, NOW, &own()).expect("a notice")
}

/// The events that need no row: the merges, the upserts the notice carries and the line.
fn notice(update: &GroupUpdate) -> Vec<BackendEvent> {
    noticed(update).events
}

/// The acts the group's row decides.
fn acts(update: &GroupUpdate) -> Vec<Act> {
    noticed(update).acts
}

/// The one message of the events, and its notice.
fn message_of(events: &[BackendEvent]) -> (&IncomingMessage, &SystemEvent) {
    let found = events.iter().find_map(|event| match event {
        BackendEvent::Message(message) => Some(message),
        _ => None,
    });
    let message = found.expect("a message among the events");
    let Body::System(system) = &message.body else {
        panic!("not a notice: {:?}", message.body)
    };
    (message, system)
}

/// Every event of the row passes the contract's own check, as a durable envelope.
fn checked(events: &[BackendEvent]) {
    let (envelope, _waiter) = Envelope::durable(events.to_vec());
    envelope.validate().expect("a valid envelope");
}

#[test]
fn a_group_update_is_a_notice_with_a_stable_id() {
    let ada = format!("{ADA_LID}@lid");
    let action = || Action::Subject {
        subject: "A synthetic name".to_owned(),
        subject_owner: None,
        subject_owner_pn: None,
        subject_owner_username: None,
        subject_time: None,
    };
    let first = notice(&update(Some(&ada), action()));
    let again = notice(&update(Some(&ada), action()));
    checked(&first);
    let (message, system) = message_of(&first);
    assert_eq!(&*message.local, "gp:3178000001:0");
    assert_eq!(
        message.conv,
        ConvKey::Group("wag:120363000000000001".into())
    );
    assert_eq!(
        message.origin,
        Sender::Remote {
            author: lid(ADA_LID),
            device: None
        }
    );
    assert_eq!(
        *system,
        SystemEvent::TitleChanged {
            by: Some(lid(ADA_LID)),
            title: "A synthetic name".to_owned()
        }
    );
    // The notification's time, both as the sender's and as the server's.
    assert_eq!(message.sent_at, 1_700_000_100_000);
    assert_eq!(message.server_at, Some(1_700_000_100_000));
    assert_eq!(message.received_at, i64::try_from(NOW).unwrap());
    // The redelivery is the same row, the next action of the notification another one.
    assert_eq!(message_of(&again).0.local, message.local);
    let mut second = update(Some(&ada), action());
    second.action_index = 1;
    assert_eq!(&*message_of(&notice(&second)).0.local, "gp:3178000001:1");
    // Without an id the notification is named by its time.
    let mut nameless = update(Some(&ada), action());
    nameless.notification_id = None;
    assert_eq!(&*message_of(&notice(&nameless)).0.local, "gp:1700000100:0");
    // An id the contract refuses gives the same fallback instead of losing the notice.
    let mut odd = update(Some(&ada), action());
    odd.notification_id = Some("bad\nid".to_owned());
    assert_eq!(&*message_of(&notice(&odd)).0.local, "gp:1700000100:0");
}

#[test]
fn members_join_are_added_leave_and_are_removed() {
    let ada = format!("{ADA_LID}@lid");
    let bob = format!("{BOB_LID}@lid");
    let ours = format!("{OUR_LID}@lid");
    let add = |participants: Vec<GroupParticipantInfo>| Action::Add {
        participants,
        reason: None,
    };
    let remove = |participants: Vec<GroupParticipantInfo>| Action::Remove {
        participants,
        reason: None,
    };
    // Ada adds Bob.
    let events = notice(&update(Some(&ada), add(vec![member(&bob)])));
    checked(&events);
    assert_eq!(
        *message_of(&events).1,
        SystemEvent::MembersAdded {
            by: Some(lid(ADA_LID)),
            users: vec![lid(BOB_LID)]
        }
    );
    // Somebody joins of his own accord: the viewmodel reads `by == the only user` as "joined".
    let events = notice(&update(Some(&bob), add(vec![member(&bob), member(&bob)])));
    assert_eq!(
        *message_of(&events).1,
        SystemEvent::MembersAdded {
            by: Some(lid(BOB_LID)),
            users: vec![lid(BOB_LID)]
        },
        "a person named twice is one"
    );
    // We add Ada: `by: None` is "You", the origin is our other device.
    let events = notice(&update(Some(&ours), add(vec![member(&ada)])));
    let (message, system) = message_of(&events);
    assert_eq!(
        *system,
        SystemEvent::MembersAdded {
            by: None,
            users: vec![lid(ADA_LID)]
        }
    );
    assert_eq!(message.origin, Sender::OwnOtherDevice { device: None });
    // We join by a link: the account's own join, as Signal's.
    let events = notice(&update(Some(&ours), add(vec![member(&ours)])));
    assert_eq!(*message_of(&events).1, SystemEvent::Joined);
    // Ada removes Bob; Bob leaves; the participant list may be empty (nothing to say).
    let events = notice(&update(Some(&ada), remove(vec![member(&bob)])));
    assert_eq!(
        *message_of(&events).1,
        SystemEvent::MembersRemoved {
            by: Some(lid(ADA_LID)),
            users: vec![lid(BOB_LID)]
        }
    );
    let events = notice(&update(Some(&bob), remove(vec![member(&bob)])));
    assert_eq!(
        *message_of(&events).1,
        SystemEvent::MemberLeft { user: lid(BOB_LID) }
    );
    assert_eq!(
        group_notice(&update(Some(&ada), remove(vec![])), NOW, &own()),
        None
    );
}

#[test]
fn a_description_a_creation_and_a_timer_are_notices_and_the_timer_sets_the_chat() {
    let ada = format!("{ADA_LID}@lid");
    let events = notice(&update(
        Some(&ada),
        Action::Description {
            id: "d1".to_owned(),
            description: Some("synthetic".to_owned()),
        },
    ));
    assert_eq!(
        *message_of(&events).1,
        SystemEvent::DescriptionChanged {
            by: Some(lid(ADA_LID))
        }
    );
    let raw = wacore_binary::builder::NodeBuilder::new("create").build();
    let events = notice(&update(Some(&ada), Action::Create { raw }));
    assert_eq!(
        *message_of(&events).1,
        SystemEvent::GroupCreated {
            by: Some(lid(ADA_LID))
        }
    );
    // A timer: the chat's upsert (with the notification's time as its version) and then the notice.
    let events = notice(&update(
        Some(&ada),
        Action::Ephemeral {
            expiration: 86_400,
            trigger: None,
        },
    ));
    checked(&events);
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(message),
    ] = events.as_slice()
    else {
        panic!("not an upsert and a notice: {events:?}")
    };
    assert_eq!(patch.key, ConvKey::Group("wag:120363000000000001".into()));
    assert_eq!(
        patch.timer,
        Patch::Set(std::time::Duration::from_secs(86_400))
    );
    assert_eq!(patch.timer_version, Some(1_700_000_100));
    assert_eq!(
        message.body,
        Body::System(SystemEvent::TimerChanged {
            by: Some(lid(ADA_LID)),
            timer: Some(std::time::Duration::from_secs(86_400))
        })
    );
    // The notice carries no timer of its own (F1: a message carries only its own).
    assert_eq!(message.expiry, None);
    // Off: the timer is cleared and the notice says none.
    let events = notice(&update(
        Some(&ada),
        Action::Ephemeral {
            expiration: 0,
            trigger: None,
        },
    ));
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(message),
    ] = events.as_slice()
    else {
        panic!("{events:?}")
    };
    assert_eq!(patch.timer, Patch::Clear);
    assert_eq!(
        message.body,
        Body::System(SystemEvent::TimerChanged {
            by: Some(lid(ADA_LID)),
            timer: None
        })
    );
}

#[test]
fn what_has_no_notice_kind_gives_no_line_and_a_change_without_an_actor_gives_no_line() {
    let ada = format!("{ADA_LID}@lid");
    let promote = Action::Promote {
        participants: vec![member(&format!("{BOB_LID}@lid"))],
    };
    // The contract has no kind of line for a role, a rule or a deletion: the acts carry what changed (W6a2, D9).
    for action in [
        promote,
        Action::Locked { threshold: None },
        Action::Announce,
        Action::NotAnnounce,
        Action::MemberAddMode {
            mode: "admin_add".to_owned(),
        },
        Action::Delete { reason: None },
    ] {
        let noticed = noticed(&update(Some(&ada), action));
        assert!(noticed.events.is_empty(), "no line: {:?}", noticed.events);
        assert!(!noticed.acts.is_empty());
    }
    // Invites and the rest are K8c's: nothing at all.
    assert_eq!(
        group_notice(
            &update(
                Some(&ada),
                Action::Invite {
                    code: "x".to_owned()
                }
            ),
            NOW,
            &own()
        ),
        None
    );
    // A change the server made itself: no author to name (`by: None` would read "You"). Its title and timer still
    // reach the chat.
    let subject = Action::Subject {
        subject: "x".to_owned(),
        subject_owner: None,
        subject_owner_pn: None,
        subject_owner_username: None,
        subject_time: None,
    };
    // A server-made rename has no line and still renames the chat (fix F11): the act, dated by the notification.
    let made = noticed(&update(None, subject));
    assert!(made.events.is_empty(), "{:?}", made.events);
    assert_eq!(
        made.acts,
        vec![Act::Title {
            title: "x".to_owned(),
            at_s: 1_700_000_100
        }]
    );
    let events = notice(&update(
        None,
        Action::Ephemeral {
            expiration: 604_800,
            trigger: None,
        },
    ));
    assert!(
        matches!(events.as_slice(), [BackendEvent::ConversationUpsert(_)]),
        "{events:?}"
    );
    // A notification of a chat that is not a group.
    let mut elsewhere = update(
        Some(&ada),
        Action::Subject {
            subject: "x".to_owned(),
            subject_owner: None,
            subject_owner_pn: None,
            subject_owner_username: None,
            subject_time: None,
        },
    );
    elsewhere.group_jid = jid("31000000009@s.whatsapp.net");
    assert_eq!(group_notice(&elsewhere, NOW, &own()), None);
}

#[test]
fn the_pairs_a_notification_vouches_for_come_first_and_never_name_us() {
    let ada = format!("{ADA_LID}@lid");
    let mut change = update(
        Some(&ada),
        Action::Add {
            participants: vec![
                {
                    let mut bob = member(&format!("{BOB_LID}@lid"));
                    bob.phone_number = Some(jid("31000000003@s.whatsapp.net"));
                    bob
                },
                {
                    // A member listed by his NUMBER whose entry carries his LID (fix F9; the
                    // `lid` attribute of a participant).
                    let mut carol = member("31000000004@s.whatsapp.net");
                    carol.lid = Some(jid("99000000004@lid"));
                    carol
                },
            ],
            reason: None,
        },
    );
    change.participant_pn = Some(jid(&format!("{ADA_PN}@s.whatsapp.net")));
    let events = notice(&change);
    checked(&events);
    let merged: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::IdentityMerged { from, into } => Some((from.clone(), into.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        merged,
        [
            (UserId::from_parts("pn", ADA_PN).unwrap(), lid(ADA_LID)),
            (
                UserId::from_parts("pn", "31000000003").unwrap(),
                lid(BOB_LID)
            ),
            (
                UserId::from_parts("pn", "31000000004").unwrap(),
                lid("99000000004")
            ),
        ],
        "the actor's number, a member's phone_number, and a member's lid attribute"
    );
    assert!(
        matches!(events[0], BackendEvent::IdentityMerged { .. })
            && matches!(events[3], BackendEvent::Message(_)),
        "merges first, the notice last"
    );
    // The same pair twice is one; our own ids pair with nobody.
    let mut ours = update(
        Some(&format!("{OUR_LID}@lid")),
        Action::Invite {
            code: "x".to_owned(),
        },
    );
    ours.participant_pn = Some(jid(&format!("{OUR_PN}@s.whatsapp.net")));
    assert_eq!(
        group_notice(&ours, NOW, &own()),
        None,
        "no merge of our own ids, no notice kind"
    );
    let mut twice = update(
        Some(&ada),
        Action::Subject {
            subject: "x".to_owned(),
            subject_owner: None,
            subject_owner_pn: None,
            subject_owner_username: None,
            subject_time: None,
        },
    );
    twice.participant_pn = Some(jid(&format!("{ADA_PN}@s.whatsapp.net")));
    let merges = notice(&twice)
        .iter()
        .filter(|event| matches!(event, BackendEvent::IdentityMerged { .. }))
        .count();
    assert_eq!(merges, 1);
}

// ----- the placeholder of a message that cannot be read (C6c) -----

fn lost(
    chat: &str,
    sender: &str,
    from_me: bool,
    mode: DecryptFailMode,
    unavailable: Option<UnavailableType>,
) -> UndecryptableMessage {
    UndecryptableMessage::builder()
        .info(Arc::new(MessageInfo {
            id: "3EB0DEAD".into(),
            source: MessageSource {
                chat: jid(chat),
                sender: jid(sender),
                is_from_me: from_me,
                is_group: chat.ends_with("@g.us"),
                ..MessageSource::default()
            },
            timestamp: time(1_700_000_200),
            ..MessageInfo::default()
        }))
        .is_unavailable(unavailable.is_some())
        .unavailable_type(unavailable.unwrap_or_default())
        .decrypt_fail_mode(mode)
        .build()
}

fn placeholder(lost: &UndecryptableMessage) -> (IncomingMessage, Placeholder) {
    let events = undecryptable(lost, NOW).expect("a placeholder");
    checked(&events);
    let [BackendEvent::Message(message)] = events.as_slice() else {
        panic!("not one message: {events:?}")
    };
    let Body::Placeholder(placeholder) = &message.body else {
        panic!("not a placeholder: {:?}", message.body)
    };
    (message.clone(), *placeholder)
}

#[test]
fn an_undecryptable_message_is_a_placeholder_under_its_own_id() {
    let (message, placeholder) = placeholder(&lost(
        "31000000002@s.whatsapp.net",
        "31000000002@s.whatsapp.net",
        false,
        DecryptFailMode::Show,
        None,
    ));
    assert_eq!(
        &*message.local, "3EB0DEAD",
        "the real message replaces it by the id"
    );
    assert_eq!(
        message.conv,
        ConvKey::Direct(UserId::from_parts("pn", ADA_PN).unwrap())
    );
    assert_eq!(
        message.origin,
        Sender::Remote {
            author: UserId::from_parts("pn", ADA_PN).unwrap(),
            device: None
        }
    );
    // The sender may still send it again: that is what the retry receipt upstream sends asks for.
    assert_eq!(placeholder, Placeholder::Undecryptable { resendable: true });
    assert_eq!(message.sent_at, 1_700_000_200_000);
    assert_eq!(message.received_at, i64::try_from(NOW).unwrap());
    // A group member, with his device.
    let (message, _) = placeholder_in_group();
    assert_eq!(
        message.conv,
        ConvKey::Group("wag:120363000000000001".into())
    );
    assert_eq!(
        message.origin,
        Sender::Remote {
            author: lid(ADA_LID),
            device: Some(4)
        }
    );
}

fn placeholder_in_group() -> (IncomingMessage, Placeholder) {
    placeholder(&lost(
        GROUP,
        &format!("{ADA_LID}:4@lid"),
        false,
        DecryptFailMode::Show,
        None,
    ))
}

#[test]
fn a_hidden_failure_is_nothing_and_an_unavailable_message_is_not_resendable() {
    // WhatsApp marks the failures of a reaction, an edit and the other protocol messages `hide`.
    assert_eq!(
        undecryptable(
            &lost(
                "31000000002@s.whatsapp.net",
                "31000000002@s.whatsapp.net",
                false,
                DecryptFailMode::Hide,
                None
            ),
            NOW
        ),
        None
    );
    // View-once, hosted and bot messages never come to a companion: the placeholder promises no resend.
    for kind in [
        UnavailableType::ViewOnce,
        UnavailableType::Hosted,
        UnavailableType::Bot,
    ] {
        let (_, placeholder) = placeholder(&lost(
            "31000000002@s.whatsapp.net",
            "31000000002@s.whatsapp.net",
            false,
            DecryptFailMode::Show,
            Some(kind),
        ));
        assert_eq!(
            placeholder,
            Placeholder::Undecryptable { resendable: false }
        );
    }
    // Our own phone's message, the status feed and a sender that is no person: nothing.
    for (chat, sender, from_me) in [
        (
            "31000000002@s.whatsapp.net",
            "15550100123@s.whatsapp.net",
            true,
        ),
        ("status@broadcast", "31000000002@s.whatsapp.net", false),
        (GROUP, GROUP, false),
    ] {
        assert_eq!(
            undecryptable(
                &lost(chat, sender, from_me, DecryptFailMode::Show, None),
                NOW
            ),
            None
        );
    }
}

/// Fix F11 (the lead's Q8.3) and M5: a rename is the chat's title as well as a line. The title is an act the group's row
/// decides (an older rename never sets an older title), dated by the notice's `s_t`, else by the notification's time; the
/// line comes with it. The title is capped like the line's, and a blank name renames nothing (the line is still told).
/// Mutant: the title dated 0 when `s_t` is absent.
#[test]
fn a_rename_is_a_dated_title_act_and_a_line() {
    let ada = format!("{ADA_LID}@lid");
    let subject = |text: &str, at: Option<u64>| Action::Subject {
        subject: text.to_owned(),
        subject_owner: None,
        subject_owner_pn: None,
        subject_owner_username: None,
        subject_time: at,
    };
    let renamed = noticed(&update(Some(&ada), subject("A new synthetic name", None)));
    checked(&renamed.events);
    let [BackendEvent::Message(message)] = renamed.events.as_slice() else {
        panic!("only the line: {:?}", renamed.events)
    };
    assert_eq!(
        message.body,
        Body::System(SystemEvent::TitleChanged {
            by: Some(lid(ADA_LID)),
            title: "A new synthetic name".to_owned()
        })
    );
    // No `s_t`: the notification's time (1_700_000_100) dates the title (scout T6), never 0.
    assert_eq!(
        renamed.acts,
        vec![Act::Title {
            title: "A new synthetic name".to_owned(),
            at_s: 1_700_000_100
        }]
    );
    // An `s_t` wins: the time the server says the rename was made, which a delayed notice may not be.
    assert_eq!(
        acts(&update(Some(&ada), subject("Older", Some(1_699_000_000)))),
        vec![Act::Title {
            title: "Older".to_owned(),
            at_s: 1_699_000_000
        }]
    );
    // The cap of the line is the cap of the title.
    let long = "x".repeat(TITLE_CAP + 40);
    assert_eq!(
        acts(&update(Some(&ada), subject(&long, None))),
        vec![Act::Title {
            title: "x".repeat(TITLE_CAP),
            at_s: 1_700_000_100
        }]
    );
    // An empty or blank name renames nothing; the line is still told.
    for blank in ["", "   "] {
        let blank = noticed(&update(Some(&ada), subject(blank, None)));
        assert!(blank.acts.is_empty(), "{:?}", blank.acts);
        assert!(matches!(
            blank.events.as_slice(),
            [BackendEvent::Message(_)]
        ));
    }
}

/// D9, D10 and scout T1: a promote or a demote is the ACTION's role whatever the participant's `type` says (upstream reads
/// `type` and says `Participant` when the server wrote none); no line; and a participant that is one of ours, by his id,
/// his number or his LID, also moves OUR place.
/// Mutant: the role read from `type`.
#[test]
fn a_promote_and_a_demote_are_the_actions_role_and_ours_moves_our_place() {
    use wacore::stanza::groups::GroupParticipantType;
    let ada = format!("{ADA_LID}@lid");
    let bob = format!("{BOB_LID}@lid");
    let typed = |text: &str, kind: GroupParticipantType| {
        let mut info = member(text);
        info.r#type = Some(kind);
        info
    };
    let admin = |user: &str| Member {
        user: lid(user),
        role: MemberRole::Admin,
    };
    let plain = |user: &str| Member {
        user: lid(user),
        role: MemberRole::Member,
    };
    // The server writes `participant` on a promote (upstream's default for a missing attribute): still an admin.
    let promoted = noticed(&update(
        Some(&ada),
        Action::Promote {
            participants: vec![typed(&bob, GroupParticipantType::Participant)],
        },
    ));
    assert!(promoted.events.is_empty());
    assert_eq!(
        promoted.acts,
        vec![Act::Members {
            upsert: vec![admin(BOB_LID)],
            remove: vec![]
        }]
    );
    // A demote whose `type` says admin: still a member.
    assert_eq!(
        acts(&update(
            Some(&ada),
            Action::Demote {
                participants: vec![typed(&bob, GroupParticipantType::Admin)],
            },
        )),
        vec![Act::Members {
            upsert: vec![plain(BOB_LID)],
            remove: vec![]
        }]
    );
    // Us, named by the number only (the list may name us by either form).
    let mut by_number = member(&format!("{ADA_LID}@lid"));
    by_number.phone_number = Some(jid(&format!("{OUR_PN}@s.whatsapp.net")));
    assert_eq!(
        acts(&update(
            Some(&ada),
            Action::Promote {
                participants: vec![by_number.clone()]
            }
        ))
        .last(),
        Some(&Act::Rule(Change::OwnAdmin(true)))
    );
    assert_eq!(
        acts(&update(
            Some(&ada),
            Action::Demote {
                participants: vec![member(&format!("{OUR_LID}@lid"))]
            }
        ))
        .last(),
        Some(&Act::Rule(Change::OwnAdmin(false)))
    );
    // Somebody else's promote does not move our place.
    assert!(
        !promoted
            .acts
            .iter()
            .any(|act| matches!(act, Act::Rule(Change::OwnAdmin(_))))
    );
}

/// D9: joins and removals carry a member delta; one that names us also says we are back or gone; a group the whole-list
/// of which the core lacks is told by the zone, not here.
/// Mutant: our removal gives no `Gone`.
#[test]
fn a_join_and_a_removal_carry_a_delta_and_the_one_that_names_us_moves_our_place() {
    let ada = format!("{ADA_LID}@lid");
    let bob = format!("{BOB_LID}@lid");
    let ours = format!("{OUR_LID}@lid");
    let add = |participants: Vec<GroupParticipantInfo>| Action::Add {
        participants,
        reason: None,
    };
    let remove = |participants: Vec<GroupParticipantInfo>| Action::Remove {
        participants,
        reason: None,
    };
    assert_eq!(
        acts(&update(Some(&ada), add(vec![member(&bob)]))),
        vec![Act::Members {
            upsert: vec![Member {
                user: lid(BOB_LID),
                role: MemberRole::Member
            }],
            remove: vec![]
        }]
    );
    let back = acts(&update(Some(&ada), add(vec![member(&bob), member(&ours)])));
    assert_eq!(back.last(), Some(&Act::Back));
    assert_eq!(back.len(), 2);
    assert_eq!(
        acts(&update(Some(&ada), remove(vec![member(&bob)]))),
        vec![Act::Members {
            upsert: vec![],
            remove: vec![lid(BOB_LID)]
        }]
    );
    // We are removed, named by our number only; and we leave by ourselves.
    let mut by_number = member(&format!("{ADA_LID}@lid"));
    by_number.phone_number = Some(jid(&format!("{OUR_PN}@s.whatsapp.net")));
    assert_eq!(
        acts(&update(Some(&ada), remove(vec![by_number]))).last(),
        Some(&Act::Gone)
    );
    assert_eq!(
        acts(&update(Some(&ours), remove(vec![member(&ours)]))).last(),
        Some(&Act::Gone)
    );
}

/// D9, D10: the rules a notice tells are the row's acts; a mode it does not know, a member's new number, the community
/// family, `create` and a stanza whose participant list was incomplete ask for a fetch; a deletion ends the group.
/// Mutant: `announcement` gives no act.
#[test]
fn rules_are_acts_and_what_a_notice_cannot_carry_asks_for_a_fetch() {
    let ada = format!("{ADA_LID}@lid");
    let raw = || wacore_binary::builder::NodeBuilder::new("x").build();
    for (n, (action, want)) in [
        (Action::Announce, Act::Rule(Change::Announcements(true))),
        (Action::NotAnnounce, Act::Rule(Change::Announcements(false))),
        (
            Action::Locked { threshold: None },
            Act::Rule(Change::Locked(true)),
        ),
        (Action::Unlocked, Act::Rule(Change::Locked(false))),
        (Action::Suspended, Act::Rule(Change::Suspended(true))),
        (Action::Unsuspended, Act::Rule(Change::Suspended(false))),
        (
            Action::MemberAddMode {
                mode: "admin_add".to_owned(),
            },
            Act::Rule(Change::MembersAdminOnly(true)),
        ),
        (
            Action::MemberAddMode {
                mode: "all_member_add".to_owned(),
            },
            Act::Rule(Change::MembersAdminOnly(false)),
        ),
        (
            Action::MemberAddMode {
                mode: "something_new".to_owned(),
            },
            Act::Fetch,
        ),
        (
            Action::Modify {
                participants: vec![member(&format!("{BOB_LID}@lid"))],
            },
            Act::Fetch,
        ),
        (
            Action::Link {
                link_type: "x".to_owned(),
                raw: raw(),
            },
            Act::Fetch,
        ),
        (
            Action::Unlink {
                unlink_type: "x".to_owned(),
                unlink_reason: None,
                raw: raw(),
            },
            Act::Fetch,
        ),
        (
            Action::LinkedGroupPromote {
                participants: vec![],
            },
            Act::Fetch,
        ),
        (
            Action::LinkedGroupDemote {
                participants: vec![],
            },
            Act::Fetch,
        ),
        (Action::Delete { reason: None }, Act::Deleted),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(acts(&update(Some(&ada), action)), vec![want], "case {n}");
    }
    // `create`: the line, and a fetch (the notice carries the group only as a raw node).
    let created = noticed(&update(Some(&ada), Action::Create { raw: raw() }));
    assert_eq!(created.acts, vec![Act::Fetch]);
    assert!(matches!(
        created.events.as_slice(),
        [BackendEvent::Message(_)]
    ));
    // The server says the participant list was incomplete: any action asks for the fetch, once.
    let mut partial = update(
        Some(&ada),
        Action::Add {
            participants: vec![member(&format!("{BOB_LID}@lid"))],
            reason: None,
        },
    );
    partial.has_incomplete_participant_information = true;
    assert_eq!(acts(&partial).last(), Some(&Act::Fetch));
    let mut twice = update(Some(&ada), Action::Create { raw: raw() });
    twice.has_incomplete_participant_information = true;
    assert_eq!(acts(&twice), vec![Act::Fetch]);
}

/// D9: a description's text is carried by the notice (set, or cleared by a deletion or a blank text), cut at the cap; the
/// line is told with it, after the upsert.
/// Mutant: a deletion keeps the old text.
#[test]
fn a_description_is_carried_by_the_notice_and_a_deletion_clears_it() {
    let ada = format!("{ADA_LID}@lid");
    let described = |text: Option<&str>| {
        notice(&update(
            Some(&ada),
            Action::Description {
                id: "d1".to_owned(),
                description: text.map(str::to_owned),
            },
        ))
    };
    let description = |events: &[BackendEvent]| match events {
        [
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(_),
        ] => patch.description.clone(),
        other => panic!("an upsert and the line: {other:?}"),
    };
    assert_eq!(
        description(&described(Some("synthetic words"))),
        Patch::Set("synthetic words".to_owned())
    );
    assert_eq!(description(&described(None)), Patch::Clear);
    assert_eq!(description(&described(Some("  "))), Patch::Clear);
    assert_eq!(
        description(&described(Some(&"y".repeat(DESCRIPTION_CAP + 10)))),
        Patch::Set("y".repeat(DESCRIPTION_CAP))
    );
}

/// W6a2 B8a (D8): the actor's own name (`notify`) rides with a notice that made something, after the merges; never ours, never
/// for a change the server made itself, never the placeholder, and a notice that makes nothing stays nothing.
/// Mutants: our own name stated; a name for a notice with no actor; "-" kept; a nameless notice made into one.
#[test]
fn the_actors_name_rides_with_a_notice_that_made_something_and_never_ours_or_a_placeholder() {
    let ada = format!("{ADA_LID}@lid");
    let ours = format!("{OUR_LID}@lid");
    let promote = || Action::Promote {
        participants: vec![member(&format!("{BOB_LID}@lid"))],
    };
    let named = |actor: Option<&str>, notify: Option<&str>, action: Action| {
        let mut u = update(actor, action);
        u.notify = notify.map(str::to_owned);
        group_notice(&u, NOW, &own())
    };
    let names = |noticed: &Noticed| -> Vec<(UserId, String)> {
        noticed
            .events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::ContactUpsert(patch) => match &patch.profile_name {
                    Patch::Set(name) => Some((patch.user.clone(), name.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };

    // A promote by Ada: her name is stated for her, in front of the rest.
    let with_name = named(Some(&ada), Some("  Ada Example "), promote()).expect("a notice");
    assert_eq!(
        names(&with_name),
        [(lid(ADA_LID), "Ada Example".to_owned())]
    );
    checked(&with_name.events);
    // The same notice without a name states none; the placeholder is none.
    assert!(names(&named(Some(&ada), None, promote()).expect("a notice")).is_empty());
    assert!(names(&named(Some(&ada), Some("-"), promote()).expect("a notice")).is_empty());
    // Our own act: our name is not a contact. A change the server made itself has no actor to name.
    assert!(
        names(&named(Some(&ours), Some("My Own Name"), promote()).expect("a notice")).is_empty()
    );
    assert!(names(&named(None, Some("Nobody Example"), promote()).expect("a notice")).is_empty());
    // A notice this build leaves out (an invitation change) stays nothing, name or not.
    assert!(
        named(Some(&ada), Some("Ada Example"), Action::GrowthUnlocked).is_none(),
        "a name alone makes no notice"
    );
}
