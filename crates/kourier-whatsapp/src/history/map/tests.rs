//! The history map (W4, C9): a message of a chunk takes the live road's ids, says it is read, tells the
//! state of an own message and keeps what this build cannot show. Every number and id is synthetic
//! (the fictitious 555-01xx range).

use kourier_backend_api::{BackendEvent, Envelope, IncomingMessage};
use kourier_model::{ConvKey, DisappearKind, FailReason, Origin as Sender, OutgoingState, UserId};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::{Item, MAX_REACTIONS, Place, Target, message};
use crate::history::testing::{Chat, Msg, Status};
use crate::inbox::{InboxMeta, Origin};
use crate::map::{Ctx, OwnIds, Row};

const PEER: &str = "15550100101@s.whatsapp.net";
const PEER_LID: &str = "99900100101@lid";
const MEMBER: &str = "15550100102@s.whatsapp.net";
const MEMBER_LID: &str = "99900100102@lid";
const GROUP: &str = "120363000000000001@g.us";
const OWN_LID: &str = "99900100999@lid";

/// "Now" for the map: after every message time below.
const NOW: i64 = 1_700_000_500_000;
/// The time (s) of the messages below.
const AT: u64 = 1_700_000_100;

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(UserId::from_parts("lid", "99900100999").expect("lid")),
            pn: Some(UserId::from_parts("pn", "15550100999").expect("pn")),
        },
    }
}

/// The place of a chat that has only an id.
fn place(id: &str) -> Place {
    Place::of(&Chat::new(id).conversation)
}

/// The row of a message, which must be a message.
fn shown(place: &Place, msg: &Msg) -> IncomingMessage {
    match message(place, &msg.info, &ctx(), NOW) {
        Item::Message(event) => *event,
        other => panic!("not a message: {other:?}"),
    }
}

/// What the core keys a message by: (conversation, author or "" for our own, local).
fn key_of(m: &IncomingMessage) -> (String, String, String) {
    let who = match &m.origin {
        Sender::Remote { author, .. } => author.as_str().to_owned(),
        Sender::OwnOtherDevice { .. } => String::new(),
        other => panic!("not a sender a message of the network has: {other:?}"),
    };
    (m.conv.encode(), who, m.local.to_string())
}

/// The key of the same message arriving LIVE: through the inbox row and `map_row`.
fn live_key(
    chat: &str,
    sender: &str,
    id: &str,
    from_me: bool,
    is_group: bool,
) -> (String, String, String) {
    let meta = InboxMeta {
        v: InboxMeta::VERSION,
        timestamp: 1_700_000_100,
        from_me,
        is_group,
        push_name: String::new(),
        lid_addressing: false,
        sender_alt: None,
        recipient_alt: None,
        ephemeral_expiration: None,
        is_offline: false,
    };
    let mut body = wa::Message::default();
    body.conversation = Some("a message".to_owned());
    let row = Row {
        arrival: 1,
        chat: chat.into(),
        sender: sender.into(),
        id: id.into(),
        origin: Origin::E2e,
        received_at: 1_700_000_200_000,
        catch_up: false,
        message: waproto::codec::message_to_vec(&body),
        meta: serde_json::to_vec(&meta).expect("meta"),
        attempts: 0,
        aged_from: 0,
    };
    let mapped = crate::map::map_row(&row, &ctx()).expect("maps");
    match mapped.events.last() {
        Some(BackendEvent::Message(m)) => key_of(m),
        other => panic!("the live road gave no message: {other:?}"),
    }
}

fn text(chat: &str, id: &str) -> Msg {
    Msg::text(chat, id, AT, "a message")
}

fn mine(chat: &str, id: &str, status: Status) -> Msg {
    Msg::mine(chat, id, AT, "a message", status)
}

#[test]
fn a_message_has_the_same_key_live_and_in_the_history() {
    // A person's chat: the phone names it by the number, the LID beside it; the live road names it by the
    // LID once the phone knows it. The history must take the LID, or every chat of the first push appears
    // twice.
    let both = Chat::new(PEER).lid(PEER_LID).pn(PEER).conversation;
    let chat = Place::of(&both);
    assert_eq!(
        chat.target,
        Target::Conv(ConvKey::Direct(
            UserId::from_parts("lid", "99900100101").expect("lid")
        ))
    );
    let history = key_of(&shown(&chat, &text(PEER, "3EB0AA01")));
    assert_eq!(
        history,
        live_key(PEER_LID, PEER_LID, "3EB0AA01", false, false)
    );
    assert_eq!(
        history,
        (
            "direct:lid:99900100101".to_owned(),
            "lid:99900100101".to_owned(),
            "3EB0AA01".to_owned()
        )
    );

    // The LID form wins wherever it stands: a chat whose id is the LID and that states a number too.
    let lid_id = Chat::new(PEER_LID).pn(PEER).conversation;
    assert_eq!(
        Place::of(&lid_id).jid,
        PEER_LID,
        "an id that is a LID is not replaced by pnJid"
    );
    // A chat that knows only its number is the number's chat (the chunk's pair joins the two, C10).
    let number_only = Place::of(&Chat::new(PEER).conversation);
    let key = key_of(&shown(&number_only, &text(PEER, "3EB0AA02")));
    assert_eq!(key.0, "direct:pn:15550100101");
    assert_eq!(key, live_key(PEER, PEER, "3EB0AA02", false, false));

    // A group: the sender is the participant, by LID or by number, in the key or only on the message.
    let group = place(GROUP);
    for member in [MEMBER_LID, MEMBER] {
        let history = key_of(&shown(&group, &text(GROUP, "3EB0BB01").from(member)));
        assert_eq!(
            history,
            live_key(GROUP, member, "3EB0BB01", false, true),
            "participant {member}"
        );
    }
    let mut only_on_the_message = text(GROUP, "3EB0BB02").from(MEMBER_LID);
    if let Some(key) = only_on_the_message.info.key.as_option_mut() {
        key.participant = None;
    }
    assert_eq!(
        key_of(&shown(&group, &only_on_the_message)),
        live_key(GROUP, MEMBER_LID, "3EB0BB02", false, true)
    );

    // Our own message, typed on the phone: no author on either road.
    let history = key_of(&shown(&chat, &mine(PEER, "3EB0CC01", Status::READ)));
    assert_eq!(
        history,
        live_key(PEER_LID, "99900100999:2@lid", "3EB0CC01", true, false)
    );
    assert_eq!(history.1, "");
    let in_group = key_of(&shown(&group, &mine(GROUP, "3EB0CC02", Status::READ)));
    assert_eq!(
        in_group,
        live_key(GROUP, "99900100999:2@lid", "3EB0CC02", true, true)
    );
}

#[test]
fn every_history_row_says_read() {
    // WhatsApp states a chat's unread COUNT only: no row is ever "unread", whatever the phone says of the
    // message (its status), and no row notifies, counts as a catch-up or has an arrival counter.
    let chat = place(PEER);
    let mut rows = Vec::new();
    for status in [
        Status::ERROR,
        Status::PENDING,
        Status::SERVER_ACK,
        Status::DELIVERY_ACK,
        Status::READ,
        Status::PLAYED,
    ] {
        rows.push(shown(&chat, &mine(PEER, "own", status)));
        // The status of somebody else's message is the phone's own bookkeeping.
        let mut theirs = text(PEER, "theirs");
        theirs.info.status = Some(status);
        rows.push(shown(&chat, &theirs));
    }
    assert_eq!(rows.len(), 12);
    for row in &rows {
        assert!(row.read, "a history row is read, or not known to be unread");
        assert!(row.silent, "a history row never notifies");
        assert!(!row.catch_up && row.arrival.is_none());
        assert_eq!(row.sent_at, 1_700_000_100_000);
        assert_eq!(row.server_at, Some(1_700_000_100_000));
        assert_eq!(row.received_at, NOW);
    }
    // The rows of a chat are one valid batch of the contract.
    let events = vec![BackendEvent::HistoryBatch {
        conv: rows[0].conv.clone(),
        messages: rows,
        has_more: true,
        progress: Some(0.4),
    }];
    let (envelope, _waiter) = Envelope::durable(events);
    envelope.validate().expect("a valid envelope");
}

#[test]
fn own_status_and_timer() {
    let chat = place(PEER);
    let state = |status: Status| {
        shown(&chat, &mine(PEER, "own", status))
            .initial_out_state
            .expect("an own message says its state")
    };
    assert_eq!(
        state(Status::ERROR),
        OutgoingState::Failed {
            reason: FailReason::Other("failed on the phone".to_owned()),
            retryable: false
        },
        "the phone owns an old failed message: never a Retry here"
    );
    assert_eq!(
        state(Status::PENDING),
        OutgoingState::Sending { progress: None }
    );
    assert_eq!(
        state(Status::SERVER_ACK),
        OutgoingState::Sent {
            server_at: Some(1_700_000_100_000)
        }
    );
    assert_eq!(state(Status::DELIVERY_ACK), OutgoingState::Delivered);
    assert_eq!(state(Status::READ), OutgoingState::Read);
    assert_eq!(
        state(Status::PLAYED),
        OutgoingState::Viewed,
        "there is no played state"
    );
    // A message of somebody else has no state of ours.
    assert!(
        shown(&chat, &text(PEER, "theirs"))
            .initial_out_state
            .is_none()
    );

    // The timer is the message's own, started when the phone started it.
    let timed = |status: Status, started: Option<u64>| {
        shown(&chat, &mine(PEER, "own", status).timer(86_400, started)).expiry
    };
    let sent = timed(Status::DELIVERY_ACK, Some(1_700_000_050)).expect("a timer");
    assert_eq!(sent.timer.as_secs(), 86_400);
    assert_eq!(sent.kind, DisappearKind::AfterSend);
    assert_eq!(
        sent.started_at,
        Some(1_700_000_050_000),
        "the countdown goes on from where the phone had it"
    );
    // A message that was not sent keeps its timer WITHOUT a start (the contract): it is not erased for a
    // send that never happened.
    for status in [Status::PENDING, Status::ERROR] {
        let unsent = timed(status, Some(1_700_000_050)).expect("the timer stays");
        assert_eq!(unsent.timer.as_secs(), 86_400);
        assert_eq!(unsent.started_at, None, "{status:?}");
    }
    // A start in the future is replaced by the present, never kept (the contract).
    let future = timed(Status::READ, Some(1_800_000_000)).expect("a timer");
    assert_eq!(future.started_at, Some(NOW));
    // No start stated: the core decides.
    assert_eq!(timed(Status::READ, None).expect("a timer").started_at, None);
    // Somebody else's message keeps its start too; one without a timer has none, whatever the chat's.
    let theirs = shown(
        &chat,
        &text(PEER, "theirs").timer(604_800, Some(1_700_000_090)),
    );
    assert_eq!(
        theirs.expiry.expect("a timer").started_at,
        Some(1_700_000_090_000)
    );
    assert!(shown(&chat, &text(PEER, "plain")).expiry.is_none());
    assert!(
        shown(&chat, &text(PEER, "zero").timer(0, None))
            .expiry
            .is_none(),
        "a timer of no seconds is no timer"
    );
}

#[test]
fn a_stub_is_deferred_not_shown() {
    // The phone's own notices (a group changed, a number changed, a message it could not read) are kept for a
    // later build: the live notices' ids cannot be derived from a stub, so showing both would show one change
    // twice (O4).
    let chat = place(GROUP);
    let mut stub = text(GROUP, "3EB0DD01").from(MEMBER_LID);
    stub.info.message = MessageField::none();
    stub.info.message_stub_type = Some(wa::web_message_info::StubType::GROUP_CHANGE_SUBJECT);
    assert!(matches!(
        message(&chat, &stub.info, &ctx(), NOW),
        Item::Deferred("stub")
    ));
    // A stub beside content is still the phone's notice.
    let mut beside = text(GROUP, "3EB0DD02").from(MEMBER_LID);
    beside.info.message_stub_type = Some(wa::web_message_info::StubType::CIPHERTEXT);
    assert!(matches!(
        message(&chat, &beside.info, &ctx(), NOW),
        Item::Deferred("stub")
    ));
    // The default stub type says "none": a plain message is shown.
    let mut plain = text(GROUP, "3EB0DD03").from(MEMBER_LID);
    plain.info.message_stub_type = Some(wa::web_message_info::StubType::UNKNOWN);
    assert!(matches!(
        message(&chat, &plain.info, &ctx(), NOW),
        Item::Message(_)
    ));
}

#[test]
fn what_this_build_cannot_show_is_kept_and_what_is_no_message_is_let_go() {
    let chat = place(PEER);
    let item = |msg: &Msg| message(&chat, &msg.info, &ctx(), NOW);
    let with = |fill: &dyn Fn(&mut wa::Message)| {
        let mut msg = text(PEER, "3EB0EE01");
        let mut content = wa::Message::default();
        fill(&mut content);
        msg.info.message = MessageField::some(content);
        msg
    };

    // A reaction, an edit and a revoke are not rows of a batch; a poll vote is a kind a later step maps.
    let reaction =
        with(&|m| m.reaction_message = MessageField::some(wa::message::ReactionMessage::default()));
    assert!(matches!(item(&reaction), Item::Deferred("modifier")));
    let revoke = with(&|m| {
        let mut protocol = wa::message::ProtocolMessage::default();
        protocol.r#type = Some(wa::message::protocol_message::Type::REVOKE);
        m.protocol_message = MessageField::some(protocol);
    });
    assert!(matches!(item(&revoke), Item::Deferred("modifier")));
    let vote = with(&|m| {
        m.poll_update_message = MessageField::some(wa::message::PollUpdateMessage::default())
    });
    assert!(matches!(item(&vote), Item::Deferred("kind")));
    // The second upload of a picture is a part of its parent.
    let companion = with(&|m| {
        m.conversation = Some("a picture".to_owned());
        let mut association = wa::MessageAssociation::default();
        association.association_type =
            Some(wa::message_association::AssociationType::HD_IMAGE_DUAL_UPLOAD);
        let mut context = wa::MessageContextInfo::default();
        context.message_association = MessageField::some(association);
        m.message_context_info = MessageField::some(context);
    });
    assert!(matches!(item(&companion), Item::Deferred("companion")));

    // Let go: the key traffic, a notice of a history chunk, an entry with nothing in it.
    let keys = with(&|m| {
        m.sender_key_distribution_message =
            MessageField::some(wa::message::SenderKeyDistributionMessage::default());
    });
    assert!(matches!(item(&keys), Item::Skip));
    let notice = with(&|m| {
        let mut protocol = wa::message::ProtocolMessage::default();
        protocol.r#type = Some(wa::message::protocol_message::Type::HISTORY_SYNC_NOTIFICATION);
        m.protocol_message = MessageField::some(protocol);
    });
    assert!(matches!(item(&notice), Item::Skip));
    let mut empty = text(PEER, "3EB0EE02");
    empty.info.message = MessageField::none();
    assert!(matches!(item(&empty), Item::Skip));

    // A status update is let go, as live; chats this build has no conversation for are kept, not shown.
    let status = place("status@broadcast");
    assert_eq!(status.target, Target::Status);
    assert!(matches!(
        message(
            &status,
            &text("status@broadcast", "3EB0EE03").info,
            &ctx(),
            NOW
        ),
        Item::Skip
    ));
    for id in [
        "120363000000000002@newsletter",
        "12345@broadcast",
        "not an id",
    ] {
        let unmapped = place(id);
        assert_eq!(unmapped.target, Target::Unmapped, "{id}");
        assert!(matches!(
            message(&unmapped, &text(id, "3EB0EE04").info, &ctx(), NOW),
            Item::Deferred("chat")
        ));
    }

    // A message that is broken is kept too, never shown half-made.
    let mut no_key = text(PEER, "3EB0EE05");
    no_key.info.key = MessageField::none();
    assert!(matches!(item(&no_key), Item::Deferred("no key")));
    let mut no_id = text(PEER, "3EB0EE06");
    if let Some(key) = no_id.info.key.as_option_mut() {
        key.id = None;
    }
    assert!(matches!(item(&no_id), Item::Deferred("no id")));
    let mut no_time = text(PEER, "3EB0EE07");
    no_time.info.message_timestamp = None;
    assert!(matches!(item(&no_time), Item::Deferred("no time")));
    // A group message needs a sender who is a person.
    let group = place(GROUP);
    let nobody = text(GROUP, "3EB0EE08");
    assert!(matches!(
        message(&group, &nobody.info, &ctx(), NOW),
        Item::Deferred("no sender")
    ));
    let channel = text(GROUP, "3EB0EE09").from("120363000000000002@newsletter");
    assert!(matches!(
        message(&group, &channel.info, &ctx(), NOW),
        Item::Deferred("no sender")
    ));
    // Our own message needs our own id (the device row has one in a real run).
    let own = mine(PEER, "3EB0EE0A", Status::READ);
    assert!(matches!(
        message(&chat, &own.info, &Ctx::default(), NOW),
        Item::Deferred("no own id")
    ));
}

#[test]
fn reactions_come_with_their_reactors() {
    let chat = place(PEER);
    let msg = text(PEER, "3EB0FF01")
        .reaction(None, "👍", 1_700_000_200_000)
        .reaction(Some(PEER), "❤", 1_700_000_300_000);
    let got = shown(&chat, &msg).reactions;
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].by, None, "our own reaction");
    assert_eq!(got[0].emoji, "👍");
    assert_eq!(got[0].at, 1_700_000_200_000);
    assert_eq!(
        got[1].by.as_ref().map(UserId::as_str),
        Some("pn:15550100101")
    );

    // A 1:1 reaction of the other side may carry no participant: the chat's person reacted.
    let mut bare = text(PEER, "3EB0FF02").reaction(Some(PEER), "😂", 0);
    if let Some(reaction) = bare.info.reactions.first_mut()
        && let Some(key) = reaction.key.as_option_mut()
    {
        key.participant = None;
    }
    let got = shown(&chat, &bare).reactions;
    assert_eq!(
        got[0].by.as_ref().map(UserId::as_str),
        Some("pn:15550100101")
    );
    assert_eq!(got[0].at, 1_700_000_100_000, "no time: the message's");

    // A group: the reactor is the participant; one of our own ids is us; a removed reaction (no emoji) and a
    // reactor who is no person are left out.
    let group = place(GROUP);
    let msg = text(GROUP, "3EB0FF03")
        .from(MEMBER_LID)
        .reaction(Some(MEMBER_LID), "🎉", 1_700_000_200_000)
        .reaction(Some(OWN_LID), "🙏", 1_700_000_200_000)
        .reaction(Some(MEMBER), "", 1_700_000_200_000)
        .reaction(
            Some("120363000000000002@newsletter"),
            "😮",
            1_700_000_200_000,
        );
    let got = shown(&group, &msg).reactions;
    let reactors: Vec<_> = got
        .iter()
        .map(|r| (r.by.as_ref().map(UserId::as_str), r.emoji.as_str()))
        .collect();
    assert_eq!(reactors, [(Some("lid:99900100102"), "🎉"), (None, "🙏")]);

    // A hostile chunk cannot make a row heavy.
    let mut many = text(PEER, "3EB0FF04");
    for n in 0..(MAX_REACTIONS + 50) {
        many = many.reaction(None, "👍", 1_700_000_200_000 + n as i64);
    }
    assert_eq!(shown(&chat, &many).reactions.len(), MAX_REACTIONS);
}

#[test]
fn a_chat_is_placed_by_its_lid_then_its_number() {
    let lid = Target::Conv(ConvKey::Direct(
        UserId::from_parts("lid", "99900100101").expect("lid"),
    ));
    let pn = Target::Conv(ConvKey::Direct(
        UserId::from_parts("pn", "15550100101").expect("pn"),
    ));
    let cases = [
        // (id, lidJid, pnJid) -> the id the chat is placed by.
        (PEER, Some(PEER_LID), Some(PEER), PEER_LID, &lid),
        (PEER, Some(PEER_LID), None, PEER_LID, &lid),
        (PEER_LID, None, Some(PEER), PEER_LID, &lid),
        (PEER, None, Some(PEER), PEER, &pn),
        (PEER, None, None, PEER, &pn),
        // A form that is not a person's id is not trusted over the id.
        (PEER, Some("not an id"), None, PEER, &pn),
        (PEER, Some(PEER), None, PEER, &pn),
    ];
    for (id, lid_jid, pn_jid, placed_by, target) in cases {
        let mut chat = Chat::new(id);
        if let Some(form) = lid_jid {
            chat = chat.lid(form);
        }
        if let Some(form) = pn_jid {
            chat = chat.pn(form);
        }
        let got = Place::of(&chat.conversation);
        assert_eq!(got.jid, placed_by, "{id} {lid_jid:?} {pn_jid:?}");
        assert_eq!(&got.target, target);
        // A later build places a chat again from the id it stored.
        assert_eq!(Place::from_jid(&got.jid), got);
    }
    // A group has no other form; a Chat Lock is not a reason to leave a chat out (D1).
    let locked = Chat::new(GROUP).lid(PEER_LID).locked().conversation;
    let got = Place::of(&locked);
    assert_eq!(got.jid, GROUP);
    assert_eq!(
        got.target,
        Target::Conv(ConvKey::Group("wag:120363000000000001".into()))
    );
}
