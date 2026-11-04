//! The map (W3a, chunk C3): what a row becomes. Numbers and ids are the fictitious 555-01xx range.

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_model::{Body, ConvKey, DisappearKind, Origin as Sender, Placeholder};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::{Ctx, Failure, MAP_VERSION, Mapped, Row, poison_placeholder};
use crate::inbox::{InboxMeta, Origin};

mod content;
mod files;
mod modifiers;

const PEER: &str = "15550100101@s.whatsapp.net";
const PEER_LID: &str = "99900100101@lid";
const GROUP: &str = "120363000000000001@g.us";

fn meta() -> InboxMeta {
    InboxMeta {
        v: InboxMeta::VERSION,
        timestamp: 1_700_000_000,
        from_me: false,
        is_group: false,
        push_name: String::new(),
        lid_addressing: false,
        sender_alt: None,
        recipient_alt: None,
        ephemeral_expiration: None,
        is_offline: false,
    }
}

pub(super) fn text(body: &str) -> wa::Message {
    let mut message = wa::Message::default();
    message.conversation = Some(body.to_owned());
    message
}

fn row_with(message: &wa::Message, meta: &InboxMeta, chat: &str, sender: &str) -> Row {
    Row {
        arrival: 7,
        chat: chat.into(),
        sender: sender.into(),
        id: "3EB0AA01".into(),
        origin: Origin::E2e,
        received_at: 1_700_000_100_000,
        catch_up: false,
        message: waproto::codec::message_to_vec(message),
        meta: serde_json::to_vec(meta).expect("meta"),
        attempts: 0,
        aged_from: 0,
    }
}

fn row(message: &wa::Message) -> Row {
    row_with(message, &meta(), PEER, PEER)
}

/// The map with no own ids (the tests that need them build a `Ctx`).
fn map_row(row: &Row) -> Result<Mapped, Failure> {
    super::map_row(row, &Ctx::default())
}

/// Every mapped row is a valid envelope (the contract's own check: ids, call lines, durable events).
fn checked(events: &[BackendEvent]) {
    let (envelope, _waiter) = Envelope::durable(events.to_vec());
    envelope.validate().expect("the row is a valid envelope");
}

fn event(mapped: Mapped) -> kourier_backend_api::IncomingMessage {
    assert_eq!(mapped.events.len(), 1, "one event: {mapped:?}");
    checked(&mapped.events);
    match mapped.events.into_iter().next().expect("an event") {
        BackendEvent::Message(m) => m,
        other => panic!("not a message: {other:?}"),
    }
}

#[test]
fn a_text_becomes_a_message_with_its_ids_stamps_and_flags() {
    let mut r = row(&text("hello"));
    r.catch_up = true;
    let m = event(map_row(&r).expect("maps"));
    assert_eq!(m.conv.encode(), "direct:pn:15550100101");
    assert_eq!(&*m.local, "3EB0AA01");
    assert!(matches!(&m.body, Body::Text(t) if t.body == "hello"));
    assert!(matches!(
        &m.origin,
        Sender::Remote { author, device: None } if author.as_str() == "pn:15550100101"
    ));
    assert_eq!(m.sent_at, 1_700_000_000_000);
    assert_eq!(
        m.server_at,
        Some(1_700_000_000_000),
        "the core measures the edit and delete windows from the server's time"
    );
    assert_eq!(m.received_at, 1_700_000_100_000);
    assert_eq!(m.arrival, Some(7));
    assert!(m.catch_up);
    assert!(m.expiry.is_none());
    assert!(m.raw.is_none());
}

#[test]
fn a_group_message_and_an_own_message_and_a_lid_chat() {
    let mut m = meta();
    m.is_group = true;
    let sender = "99900100101:3@lid";
    let r = row_with(&text("in a group"), &m, GROUP, sender);
    let got = event(map_row(&r).expect("maps"));
    assert_eq!(got.conv, ConvKey::Group("wag:120363000000000001".into()));
    assert!(matches!(
        &got.origin,
        Sender::Remote { author, device: Some(3) } if author.as_str() == "lid:99900100101"
    ));

    let mut own = meta();
    own.from_me = true;
    let r = row_with(&text("from my phone"), &own, PEER_LID, "99900100123:2@lid");
    let got = event(map_row(&r).expect("maps"));
    assert_eq!(got.conv.encode(), "direct:lid:99900100101");
    assert!(matches!(
        &got.origin,
        Sender::OwnOtherDevice { device: Some(2) }
    ));
}

#[test]
fn other_content_is_an_unsupported_message_that_keeps_the_encoded_message() {
    let mut message = wa::Message::default();
    // A list of contact cards stays unsupported until the model has a list of cards (lead L10).
    message.contacts_array_message =
        MessageField::some(wa::message::ContactsArrayMessage::default());
    let r = row(&message);
    let m = event(map_row(&r).expect("maps"));
    assert!(matches!(&m.body, Body::Unsupported { kind_hint, .. } if kind_hint == "contacts"));
    let raw = m.raw.expect("raw");
    assert_eq!(raw.bytes, r.message, "the payload is kept as it was stored");
}

fn protocol(fill: impl FnOnce(&mut wa::message::ProtocolMessage)) -> wa::Message {
    let mut inner = wa::message::ProtocolMessage::default();
    fill(&mut inner);
    let mut message = wa::Message::default();
    message.protocol_message = MessageField::some(inner);
    message
}

#[test]
fn only_what_is_provably_no_message_is_finished_without_an_event() {
    // The one list (`Class::Inert`): a key distribution alone, the app-state key traffic, the
    // history-sync notices, the peer-data-operation request and response (upstream consumes them).
    let mut keys = wa::Message::default();
    keys.sender_key_distribution_message =
        MessageField::some(wa::message::SenderKeyDistributionMessage::default());
    let key_share = protocol(|p| {
        p.app_state_sync_key_share =
            MessageField::some(wa::message::AppStateSyncKeyShare::default());
    });
    let key_request = protocol(|p| {
        p.app_state_sync_key_request =
            MessageField::some(wa::message::AppStateSyncKeyRequest::default());
    });
    let pdo_request = protocol(|p| {
        p.peer_data_operation_request_message =
            MessageField::some(wa::message::PeerDataOperationRequestMessage::default());
    });
    let pdo_response = protocol(|p| {
        p.peer_data_operation_request_response_message =
            MessageField::some(wa::message::PeerDataOperationRequestResponseMessage::default());
    });
    // (A history-sync notice of somebody else is inert too, and one of our own phone is announced:
    // `a_history_notice_of_our_phone_is_announced_not_deferred`.)
    let peers_notice = protocol(|p| {
        p.history_sync_notification =
            MessageField::some(wa::message::HistorySyncNotification::default());
    });
    for message in [
        keys,
        peers_notice,
        key_share,
        key_request,
        pdo_request,
        pdo_response,
    ] {
        let mapped = map_row(&row(&message)).expect("maps");
        assert!(mapped.events.is_empty() && !mapped.deferred, "inert");
    }
    // A group message may carry a key distribution AND its text: that is a message.
    let mut both = text("hello with keys");
    both.sender_key_distribution_message =
        MessageField::some(wa::message::SenderKeyDistributionMessage::default());
    let got = event(map_row(&row(&both)).expect("maps"));
    assert!(matches!(&got.body, Body::Text(t) if t.body == "hello with keys"));
}

/// W4 (critic H1): the phone's notice that a chunk of the history is coming is not a message and not
/// a kind to keep: when OUR OWN phone sent it, the map says `announced` with the stanza's id (stage 2
/// records it), whichever way upstream left the protocol message (the payload field is taken out
/// before the hook, the type stays). It is finished like an inert item: no event, not deferred.
#[test]
fn a_history_notice_of_our_phone_is_announced_not_deferred() {
    use wa::message::protocol_message::Type;
    let mut ours = meta();
    ours.from_me = true;
    let by_type = protocol(|p| p.r#type = Some(Type::HISTORY_SYNC_NOTIFICATION));
    let by_payload = protocol(|p| {
        p.history_sync_notification =
            MessageField::some(wa::message::HistorySyncNotification::default());
    });
    let both = protocol(|p| {
        p.r#type = Some(Type::HISTORY_SYNC_NOTIFICATION);
        p.history_sync_notification =
            MessageField::some(wa::message::HistorySyncNotification::default());
    });
    for message in [by_type, by_payload, both] {
        // Our own phone (our account's id on both sides, the number or the LID form).
        for (chat, sender) in [(PEER_LID, PEER_LID), (PEER, PEER)] {
            let mapped = super::map_row(&row_with(&message, &ours, chat, sender), &Ctx::default())
                .expect("maps");
            assert_eq!(
                mapped.announced.as_deref(),
                Some("3EB0AA01"),
                "the stanza id"
            );
            assert!(
                mapped.events.is_empty() && !mapped.deferred,
                "no event, not kept: {mapped:?}"
            );
        }
    }
}

/// W4: a notice from anybody else announces nothing (upstream acts only on our own phone's; a spoofed
/// notice must not become a chunk that is "lost" for ever). It is plumbing: finished, no event.
#[test]
fn a_peers_history_notice_announces_nothing() {
    use wa::message::protocol_message::Type;
    let notice = protocol(|p| p.r#type = Some(Type::HISTORY_SYNC_NOTIFICATION));
    assert!(!meta().from_me, "the default test meta is a peer's");
    let mapped = map_row(&row(&notice)).expect("maps");
    assert_eq!(mapped.announced, None);
    assert!(mapped.events.is_empty() && !mapped.deferred, "inert");
    // An ordinary mapped message announces nothing either.
    assert_eq!(map_row(&row(&text("hi"))).expect("maps").announced, None);
}

/// Fix N1: a key distribution is wiped with its keys only when NOTHING else is in the message. Beside
/// any other content, one this build names or not (a pin in chat, buttons, an event have no name
/// here), it is a message: it is mapped like the same content without the distribution, its bytes
/// kept, or deferred when that kind is deferred. (The old rule looked for a text only.)
#[test]
fn a_key_distribution_beside_any_other_content_is_not_wiped_with_the_keys() {
    let keys = || MessageField::some(wa::message::SenderKeyDistributionMessage::default());
    // Alone, or with only the message's own metadata (not content): the keys are all it is.
    let mut alone = wa::Message::default();
    alone.sender_key_distribution_message = keys();
    let mut with_metadata = alone.clone();
    with_metadata.message_context_info = MessageField::some(wa::MessageContextInfo::default());
    for message in [alone, with_metadata] {
        let mapped = map_row(&row(&message)).expect("maps");
        assert!(mapped.events.is_empty() && !mapped.deferred, "inert");
    }

    // Beside a kind with no name in this map: the message it would be without the keys, bytes kept.
    let mut buttons = wa::Message::default();
    buttons.buttons_message = MessageField::some(wa::message::ButtonsMessage::default());
    let mut event_kind = wa::Message::default();
    event_kind.event_message = MessageField::some(wa::message::EventMessage::default());
    let mut empty_text = wa::Message::default();
    empty_text.conversation = Some(String::new());
    for (name, plain) in [
        ("buttons", buttons),
        ("an event", event_kind),
        ("an empty text", empty_text),
    ] {
        let mut with_keys = plain.clone();
        with_keys.sender_key_distribution_message = keys();
        let stored = row(&with_keys);
        let mapped = map_row(&stored).expect("maps");
        assert!(!mapped.deferred, "{name}");
        let got = match mapped.events.into_iter().next() {
            Some(BackendEvent::Message(m)) => m,
            other => panic!("{name}: wiped or not a message: {other:?}"),
        };
        let without = event(map_row(&row(&plain)).expect("maps"));
        assert!(
            matches!(&got.body, Body::Unsupported { .. }),
            "{name}: {:?}",
            got.body
        );
        assert_eq!(
            format!("{:?}", got.body),
            format!("{:?}", without.body),
            "{name}: the keys change nothing about the content"
        );
        assert_eq!(
            got.raw.expect("raw").bytes,
            stored.message,
            "{name}: the payload is kept as it was stored"
        );
    }

    // Beside a kind that is deferred (a pin in chat, a poll vote): deferred with it.
    let mut pin = wa::Message::default();
    pin.pin_in_chat_message = MessageField::some(wa::message::PinInChatMessage::default());
    let mut vote = wa::Message::default();
    vote.poll_update_message = MessageField::some(wa::message::PollUpdateMessage::default());
    for plain in [pin, vote] {
        let mut with_keys = plain;
        with_keys.sender_key_distribution_message = keys();
        let mapped = map_row(&row(&with_keys)).expect("maps");
        assert!(mapped.events.is_empty() && mapped.deferred, "deferred");
    }
}

#[test]
fn what_this_build_cannot_map_is_deferred_not_dropped() {
    // A poll vote, an encrypted reaction, an album parent, a pin in chat, a protocol message of a
    // type nobody maps: kept with their content for the step that maps them.
    let mut vote = wa::Message::default();
    vote.poll_update_message = MessageField::some(wa::message::PollUpdateMessage::default());
    let mut enc_reaction = wa::Message::default();
    enc_reaction.enc_reaction_message =
        MessageField::some(wa::message::EncReactionMessage::default());
    let mut album = wa::Message::default();
    album.album_message = MessageField::some(wa::message::AlbumMessage::default());
    let mut pin = wa::Message::default();
    pin.pin_in_chat_message = MessageField::some(wa::message::PinInChatMessage::default());
    for message in [protocol(|_| {}), vote, enc_reaction, album, pin] {
        let mapped = map_row(&row(&message)).expect("maps");
        assert!(mapped.events.is_empty() && mapped.deferred, "deferred");
    }
    // A newsletter's items wait for K8b.
    let mut newsletter = row(&text("channel post"));
    newsletter.origin = Origin::Newsletter;
    assert!(map_row(&newsletter).expect("maps").deferred);
    // A chat or a sender of a server kind this build does not know: deferred, not a failure
    // (no number of retries would map it). The hosted namespaces are NOT among them (fix F5).
    for (chat, sender) in [
        (PEER, "123@bot"),
        (PEER, "123@interop"),
        (PEER, "123@msgr"),
        ("123@bot", PEER),
        ("123@msgr", PEER),
        ("123@newsletter", PEER),
    ] {
        let mapped = map_row(&row_with(&text("x"), &meta(), chat, sender)).expect("maps");
        assert!(
            mapped.events.is_empty() && mapped.deferred,
            "{chat} / {sender}"
        );
    }
    assert_eq!(
        MAP_VERSION, 4,
        "a new map raises the version the deferred rows are compared with"
    );
}

/// Fix F5 (protocol review P6): a hosted device is another device of the same person, so `@hosted` reads
/// as the number's namespace and `@hosted.lid` as the LID's, in the chat, the sender, a vouched pair
/// and a group's participant (one helper, `map::scheme_of`). Before the fix every such row was deferred
/// for ever.
#[test]
fn a_hosted_device_reads_as_its_ordinary_namespace() {
    // The chat and the sender, number side.
    let got = event(
        map_row(&row_with(
            &text("from a hosted device"),
            &meta(),
            "15550100101@hosted",
            "15550100101:5@hosted",
        ))
        .expect("maps"),
    );
    assert_eq!(got.conv.encode(), "direct:pn:15550100101");
    assert!(matches!(
        &got.origin,
        Sender::Remote { author, device: Some(5) } if author.as_str() == "pn:15550100101"
    ));
    // The LID side.
    let got = event(
        map_row(&row_with(
            &text("from a hosted LID device"),
            &meta(),
            "99900100101@hosted.lid",
            "99900100101:7@hosted.lid",
        ))
        .expect("maps"),
    );
    assert_eq!(got.conv.encode(), "direct:lid:99900100101");
    assert!(matches!(
        &got.origin,
        Sender::Remote { author, device: Some(7) } if author.as_str() == "lid:99900100101"
    ));
    // A hosted member of a group, with the other id the stanza vouches for: the pair is merged first,
    // the message is the LID member's.
    let mut m = meta();
    m.is_group = true;
    m.sender_alt = Some("15550100101@hosted".into());
    let mapped = map_row(&row_with(
        &text("in a group"),
        &m,
        GROUP,
        "99900100101:2@hosted.lid",
    ))
    .expect("maps");
    checked(&mapped.events);
    assert!(
        matches!(
            mapped.events.as_slice(),
            [BackendEvent::IdentityMerged { from, into }, BackendEvent::Message(message)]
                if from.as_str() == "pn:15550100101"
                    && into.as_str() == "lid:99900100101"
                    && matches!(&message.origin, Sender::Remote { author, device: Some(2) }
                        if author.as_str() == "lid:99900100101")
        ),
        "{:?}",
        mapped.events
    );
    // The kinds of server that are no person stay as they were.
    for server in ["bot", "interop", "msgr", "newsletter"] {
        let jid: wacore_binary::Jid = format!("123@{server}").parse().expect("a jid");
        assert!(super::person_of(&jid).is_none() && super::conv_of(&jid).is_none());
    }
}

/// D1 (the user's "Let them go") and the critic's H3: a STATUS update is let go on arrival, a broadcast
/// LIST message is an ordinary message. The three rows W3a deferred under `Chat::Skip`.
#[test]
fn a_status_is_let_go_a_list_message_is_a_message_and_our_own_list_message_waits() {
    // The status feed: finished with no event and wiped, whatever it holds (a text, or a kind that
    // is deferred elsewhere).
    let status = row_with(&text("status"), &meta(), "status@broadcast", PEER);
    let mapped = map_row(&status).expect("maps");
    assert!(mapped.events.is_empty() && !mapped.deferred, "let go");
    let mut vote = wa::Message::default();
    vote.poll_update_message = MessageField::some(wa::message::PollUpdateMessage::default());
    let mapped = map_row(&row_with(&vote, &meta(), "status@broadcast", PEER)).expect("maps");
    assert!(mapped.events.is_empty() && !mapped.deferred, "let go");

    // A broadcast list's message from somebody else shows up in the direct chat with its sender.
    let list = row_with(&text("for the list"), &meta(), "1700000000@broadcast", PEER);
    let got = event(map_row(&list).expect("maps"));
    assert_eq!(got.conv.encode(), "direct:pn:15550100101");
    assert!(matches!(&got.body, Body::Text(t) if t.body == "for the list"));
    assert!(matches!(
        &got.origin,
        Sender::Remote { author, .. } if author.as_str() == "pn:15550100101"
    ));

    // Our own message to a list has no conversation in the contract: kept.
    let mut own = meta();
    own.from_me = true;
    let ours = row_with(
        &text("from my phone to a list"),
        &own,
        "1700000000@broadcast",
        "99900100123:2@lid",
    );
    let mapped = map_row(&ours).expect("maps");
    assert!(mapped.events.is_empty() && mapped.deferred, "kept");
}

#[test]
fn a_row_that_cannot_be_read_is_a_failure_with_a_log_safe_reason() {
    let mut bad = row(&text("x"));
    bad.message = vec![0xff, 0xff, 0xff, 0xff, 0xff];
    assert!(matches!(map_row(&bad), Err(Failure(_))));
    let mut bad = row(&text("x"));
    bad.meta = b"not json".to_vec();
    assert!(matches!(map_row(&bad), Err(Failure(_))));
    let bad = row_with(&text("x"), &meta(), "not a jid at all@@", PEER);
    let Err(Failure(reason)) = map_row(&bad) else {
        panic!("a chat id that does not parse must fail");
    };
    assert!(
        !reason.contains("@@") && !reason.contains("not a jid"),
        "no input in the reason"
    );
    // A sender id that does not parse is a failure; a server kind it does not map is deferred
    // (`what_this_build_cannot_map_is_deferred_not_dropped`).
    let bad = row_with(&text("x"), &meta(), PEER, "not a jid at all@@");
    assert!(map_row(&bad).is_err());
}

#[test]
fn the_timer_is_the_items_then_the_messages_own_and_nothing_else() {
    // 1. The item carried it.
    let mut m = meta();
    m.ephemeral_expiration = Some(86_400);
    let mapped = map_row(&row_with(&text("a"), &m, PEER, PEER)).expect("maps");
    let expiry = event(mapped).expiry.expect("expiry");
    assert_eq!(
        (expiry.kind, expiry.timer.as_secs()),
        (DisappearKind::AfterSend, 86_400)
    );
    assert_eq!(expiry.started_at, None);

    // 2. A replay lost it, but the stored message still carries it.
    let mut ext = wa::message::ExtendedTextMessage::default();
    ext.text = Some("b".into());
    let mut context = wa::ContextInfo::default();
    context.expiration = Some(3_600);
    ext.context_info = MessageField::some(context);
    let mut message = wa::Message::default();
    message.extended_text_message = MessageField::some(ext);
    let mapped = map_row(&row(&message)).expect("maps");
    assert_eq!(event(mapped).expiry.expect("expiry").timer.as_secs(), 3_600);

    // 3. Neither: NO expiry. There is no chat-level fallback (fix F1): a chat whose timer was
    // switched off sends messages that carry none, and a remembered timer erased them. The
    // item with a timer above and this one are the same chat.
    let mapped = map_row(&row(&text("c"))).expect("maps");
    assert!(event(mapped).expiry.is_none());

    // A timer of 0 is off.
    let mut off = meta();
    off.ephemeral_expiration = Some(0);
    let mapped = map_row(&row_with(&text("d"), &off, PEER, PEER)).expect("maps");
    assert!(event(mapped).expiry.is_none());
}

#[test]
fn the_placeholder_of_a_poison_row_is_for_a_remote_sender_only() {
    let mut bad = row(&text("x"));
    bad.message = vec![0xff; 5];
    let Some(BackendEvent::Message(m)) = poison_placeholder(&bad) else {
        panic!("a remote sender gets a placeholder");
    };
    assert!(matches!(
        m.body,
        Body::Placeholder(Placeholder::Undecryptable { resendable: false })
    ));
    assert_eq!(m.arrival, Some(7));
    // Our own message, an unreadable meta, and a chat without a conversation get none.
    let mut own = meta();
    own.from_me = true;
    assert!(poison_placeholder(&row_with(&text("x"), &own, PEER, PEER)).is_none());
    let mut unreadable = row(&text("x"));
    unreadable.meta = b"{".to_vec();
    assert!(poison_placeholder(&unreadable).is_none());
    assert!(poison_placeholder(&row_with(&text("x"), &meta(), "status@broadcast", PEER)).is_none());
}

#[test]
fn nothing_printed_names_a_person_or_a_message() {
    let r = row(&text("secret words"));
    let text = format!("{r:?}");
    for needle in ["secret words", "15550100101", "3EB0AA01"] {
        assert!(!text.contains(needle), "Debug printed {needle}");
    }
}
