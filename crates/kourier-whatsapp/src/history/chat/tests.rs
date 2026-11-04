//! The history map, chats (W4, C10): the hint once per conversation, a locked chat like any other, the pair
//! before the upsert, a group's title, timer and parent. Every number and id is synthetic.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, ConversationPatch, Patch, UserId};
use waproto::whatsapp as wa;

use super::{Chat, HintState, OLDEST_TIMER_VERSION, chat, hint};
use crate::history::map::{Item, Place, message};
use crate::history::testing::{Chat as Blob, Msg, Status};
use crate::map::{Ctx, OwnIds};

const PEER: &str = "15550100101@s.whatsapp.net";
const PEER_LID: &str = "99900100101@lid";
const GROUP: &str = "120363000000000001@g.us";
const COMMUNITY: &str = "120363000000000009@g.us";
const OWN: &str = "15550100999@s.whatsapp.net";
const OWN_LID: &str = "99900100999@lid";
const AT: u64 = 1_700_000_100;

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(UserId::from_parts("lid", "99900100999").expect("lid")),
            pn: Some(UserId::from_parts("pn", "15550100999").expect("pn")),
        },
    }
}

/// The chat of a conversation of the chunk, which must give events.
fn mapped(conversation: &wa::Conversation) -> Chat {
    chat(&Place::of(conversation), conversation, &ctx()).expect("the chat gives events")
}

/// The upsert in a chat's events (the last one).
fn upsert(events: &[BackendEvent]) -> ConversationPatch {
    match events.last() {
        Some(BackendEvent::ConversationUpsert(patch)) => patch.clone(),
        other => panic!("the entry does not end in an upsert: {other:?}"),
    }
}

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("user")
}

#[test]
fn the_hint_is_stated_once_per_conversation() {
    // The back end keeps `history_chats` (stated, read here) and asks `hint` for every chat of every chunk.
    // Two chunks name the same chat: the phone's count is stated the first time and never again, because
    // the core REPLACES a hint with the next one.
    let conversation = Blob::new(PEER).lid(PEER_LID).unread(4).conversation;
    let mut state = HintState::default();
    let mut stated = Vec::new();
    for _chunk in 0..3 {
        let c = mapped(&conversation);
        let hint = hint(c.unread, 0, state);
        state.stated |= hint.is_some();
        stated.push(hint);
    }
    assert_eq!(stated, vec![Some(4), None, None]);

    // A chat read on this computer between two chunks is not marked unread again.
    let mut state = HintState::default();
    let c = mapped(&conversation);
    state.read_here = true;
    assert_eq!(hint(c.unread, 0, state), None);

    // The messages the live path delivered and the phone also counts are rows already: subtracted.
    assert_eq!(hint(Some(5), 2, HintState::default()), Some(3));
    // Nothing is left to state: nothing is stated, and the chat is not marked stated by it.
    assert_eq!(hint(Some(2), 2, HintState::default()), None);
    assert_eq!(hint(Some(2), 9, HintState::default()), None);
    // A chunk that carries no count says nothing, and a later one that does can still say it.
    assert_eq!(hint(None, 0, HintState::default()), None);
    assert_eq!(hint(Some(0), 0, HintState::default()), None);

    // The count rides on the upsert, and only when it was asked for.
    let c = mapped(&conversation);
    assert_eq!(upsert(&c.events(Some(4))).unread_hint, Some(4));
    assert_eq!(upsert(&c.events(None)).unread_hint, None);
}

#[test]
fn a_locked_chat_is_delivered_like_any_other() {
    // His "Show like any chat" (D1): the chat behind the phone's Chat Lock is a chat. Its entry and its
    // messages are those of the same chat without the lock.
    let open = Blob::new(PEER).lid(PEER_LID).unread(1).message(Msg::text(
        PEER,
        "3EB0AA01",
        AT,
        "a message",
    ));
    let locked = open.clone().locked();
    assert_eq!(locked.conversation.locked, Some(true));
    assert_eq!(mapped(&open.conversation).events(Some(1)), {
        mapped(&locked.conversation).events(Some(1))
    });

    let place = Place::of(&locked.conversation);
    let info = &locked.conversation.messages[0].message;
    let info = info.as_option().expect("the message");
    assert!(matches!(
        message(&place, info, &ctx(), 1_700_000_500_000),
        Item::Message(_)
    ));
}

#[test]
fn a_chat_states_its_pair_before_its_upsert_and_never_names_us() {
    // The number is the chat's id and the LID stands beside it: the pair first, number into LID.
    let both = Blob::new(PEER).lid(PEER_LID).pn(PEER).active_at(AT);
    let events = mapped(&both.conversation).events(None);
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0],
        BackendEvent::IdentityMerged {
            from: user("pn", "15550100101"),
            into: user("lid", "99900100101"),
        }
    );
    assert!(matches!(events[1], BackendEvent::ConversationUpsert(_)));

    // The chat with ourselves (either of our ids) names no pair.
    for conversation in [
        Blob::new(OWN).lid(OWN_LID).active_at(AT).conversation,
        Blob::new(OWN_LID).pn(OWN).active_at(AT).conversation,
        Blob::new(PEER).lid(OWN_LID).active_at(AT).conversation,
    ] {
        let events = mapped(&conversation).events(None);
        assert_eq!(events.len(), 1, "no pair: {events:?}");
        assert!(matches!(events[0], BackendEvent::ConversationUpsert(_)));
    }

    // One form alone is no pair; a group is never a pair.
    assert_eq!(
        mapped(&Blob::new(PEER).active_at(AT).conversation)
            .events(None)
            .len(),
        1
    );
    let group = Blob::new(GROUP)
        .lid(PEER_LID)
        .pn(PEER)
        .name("A group")
        .conversation;
    assert_eq!(mapped(&group).events(None).len(), 1);
}

#[test]
fn a_group_states_its_title_and_parent_and_a_direct_chat_neither() {
    let mut group = Blob::new(GROUP).name("  A group name  ").conversation;
    group.parent_group_id = Some(COMMUNITY.to_owned());
    let patch = upsert(&mapped(&group).events(None));
    assert_eq!(patch.key, ConvKey::Group("wag:120363000000000001".into()));
    assert_eq!(patch.title, Patch::Set("  A group name  ".to_owned()));
    assert_eq!(
        patch.parent,
        Patch::Set(ConvKey::Group("wag:120363000000000009".into()))
    );

    // A name of blanks and a parent that is no group change nothing; a very long name is cut.
    let mut odd = Blob::new(GROUP).name("   ").conversation;
    odd.parent_group_id = Some(PEER.to_owned());
    let patch = upsert(&mapped(&odd).events(None));
    assert_eq!(patch.title, Patch::Unchanged);
    assert_eq!(patch.parent, Patch::Unchanged);
    let long = "x".repeat(2000);
    let patch = upsert(&mapped(&Blob::new(GROUP).name(&long).conversation).events(None));
    assert!(matches!(patch.title, Patch::Set(ref t) if t.chars().count() == 512));

    // A direct chat's `name` can be an address-book name: no title, and no parent.
    let mut direct = Blob::new(PEER)
        .lid(PEER_LID)
        .name("Somebody")
        .active_at(AT)
        .conversation;
    direct.parent_group_id = Some(COMMUNITY.to_owned());
    let patch = upsert(&mapped(&direct).events(None));
    assert_eq!(patch.title, Patch::Unchanged);
    assert_eq!(patch.parent, Patch::Unchanged);
}

#[test]
fn the_timer_is_ordered_against_the_changes_the_live_path_saw() {
    let timer = |seconds: Option<u32>, set_at: Option<i64>| {
        let mut conversation = Blob::new(GROUP).conversation;
        conversation.ephemeral_expiration = seconds;
        conversation.ephemeral_setting_timestamp = set_at;
        let patch = upsert(&mapped(&conversation).events(None));
        (patch.timer, patch.timer_version)
    };
    let week = Duration::from_secs(604_800);
    // With the time the phone set it: that time is the version.
    assert_eq!(
        timer(Some(604_800), Some(1_700_000_000)),
        (Patch::Set(week), Some(1_700_000_000))
    );
    // Without it: the oldest version, so a change that came live wins over it.
    assert_eq!(
        timer(Some(604_800), None),
        (Patch::Set(week), Some(OLDEST_TIMER_VERSION))
    );
    // Off, with its time: cleared. Off without one cannot be told from "never had one": nothing.
    assert_eq!(
        timer(Some(0), Some(1_700_000_000)),
        (Patch::Clear, Some(1_700_000_000))
    );
    assert_eq!(timer(Some(0), None), (Patch::Unchanged, None));
    assert_eq!(timer(None, Some(1_700_000_000)), (Patch::Unchanged, None));
    // A time that is no version (negative, past u32) is none.
    assert_eq!(
        timer(Some(604_800), Some(-5)),
        (Patch::Set(week), Some(OLDEST_TIMER_VERSION))
    );
}

#[test]
fn a_direct_chat_in_which_nothing_happened_is_no_chat() {
    let empty = Blob::new(PEER).lid(PEER_LID).conversation;
    assert!(chat(&Place::of(&empty), &empty, &ctx()).is_none());

    // A message, a time or an unread count makes it one; a group is a chat whatever it holds.
    for conversation in [
        Blob::new(PEER)
            .message(Msg::text(PEER, "3EB0AA01", AT, "hi"))
            .conversation,
        Blob::new(PEER).active_at(AT).conversation,
        Blob::new(PEER).unread(2).conversation,
        Blob::new(GROUP).conversation,
    ] {
        assert!(chat(&Place::of(&conversation), &conversation, &ctx()).is_some());
    }

    // The status feed and a chat this build has no conversation for give nothing.
    for id in [
        "status@broadcast",
        "120363000000000001@newsletter",
        "1555010010199@broadcast",
    ] {
        let conversation = Blob::new(id).active_at(AT).conversation;
        assert!(
            chat(&Place::of(&conversation), &conversation, &ctx()).is_none(),
            "{id}"
        );
    }
}

#[test]
fn the_live_cut_is_the_phones_last_message() {
    let cut = |conversation: wa::Conversation| mapped(&conversation).live_cut_ms;
    let mut both = Blob::new(PEER).active_at(AT + 50).conversation;
    both.last_msg_timestamp = Some(AT);
    assert_eq!(cut(both), AT as i64 * 1000);
    assert_eq!(
        cut(Blob::new(PEER).active_at(AT).conversation),
        AT as i64 * 1000
    );
    // Unknown: everything the live path delivered counts as inside the phone's count.
    assert_eq!(cut(Blob::new(GROUP).conversation), i64::MAX);

    let ended = Blob::new(GROUP).end_of_history().conversation;
    assert!(mapped(&ended).ended);
    assert!(!mapped(&Blob::new(GROUP).conversation).ended);
}

/// Fix F2: a chat of a chunk is `real` (the only thing in a chunk that proves a direct chat was started on the phone)
/// when it carries a message a person wrote, from either side; a time, an unread count, a name, a token, the
/// phone's notice of a call and its call log message do not make it so, and a group is never `real` (it needs no
/// start). Mutants: `real` always true; the call kinds not excluded.
#[test]
fn a_chat_is_real_only_when_it_carries_a_message_a_person_wrote() {
    use waproto::buffa::MessageField;

    let real = |blob: Blob| mapped(&blob.conversation).real;
    let mut stub = Msg::text(PEER, "3EB0AA02", AT, "");
    stub.info.message = MessageField::none();
    stub.info.message_stub_type = Some(wa::web_message_info::StubType::CALL_MISSED_VOICE);
    let mut record = Msg::text(PEER, "3EB0AA03", AT, "");
    let mut message = wa::Message::default();
    message.call_log_messsage = MessageField::some(wa::message::CallLogMessage::default());
    record.info.message = MessageField::some(message);

    assert!(!real(Blob::new(PEER).active_at(AT)), "a time alone");
    assert!(!real(Blob::new(PEER).unread(3)), "an unread count alone");
    assert!(
        !real(
            Blob::new(PEER)
                .name("synthetic name")
                .token(&[1, 2], AT, None)
                .active_at(AT)
        ),
        "a name and a token alone"
    );
    assert!(
        !real(Blob::new(PEER).active_at(AT).message(stub.clone())),
        "a call stub"
    );
    assert!(
        !real(Blob::new(PEER).active_at(AT).message(record.clone())),
        "a call log message"
    );
    assert!(
        real(Blob::new(PEER).message(Msg::text(PEER, "3EB0AA01", AT, "synthetic hi"))),
        "a message of the peer"
    );
    assert!(
        real(
            Blob::new(PEER)
                .message(stub)
                .message(record)
                .message(Msg::mine(
                    PEER,
                    "3EB0AA04",
                    AT,
                    "synthetic mine",
                    Status::READ
                ))
        ),
        "a message of ours beside call records"
    );
    assert!(
        !real(
            Blob::new(GROUP).message(Msg::text(GROUP, "3EB0AA05", AT, "synthetic hi").from(PEER))
        ),
        "a group is never real"
    );
}
