//! The map beside the content (W3b, chunk C4a): the mentions and formatting of a text, the quote, and the
//! pair of ids a stanza vouches for. Numbers and ids are the fictitious 555-01xx range.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::{
    AttachmentKind, Body, ConvKey, Origin as Sender, RangeKind, TargetAuthor, UserId,
};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::{GROUP, PEER, PEER_LID, checked, meta, row_with};
use crate::map::{Ctx, Mapped, OwnIds};

pub(super) const OWN_LID: &str = "99900100999";
pub(super) const OWN_PN: &str = "15550100999";
pub(super) const MEMBER_PN: &str = "15550100102@s.whatsapp.net";
pub(super) const MEMBER_LID: &str = "99900100102@lid";

pub(super) fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a user id")
}

/// The map of an account whose own ids are `OWN_LID` and `OWN_PN`.
pub(super) fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(user("lid", OWN_LID)),
            pn: Some(user("pn", OWN_PN)),
        },
    }
}

/// An extended text message with `body` and the context info `fill` builds.
fn said(body: &str, fill: impl FnOnce(&mut wa::ContextInfo)) -> wa::Message {
    let mut context = wa::ContextInfo::default();
    fill(&mut context);
    let mut ext = wa::message::ExtendedTextMessage::default();
    ext.text = Some(body.to_owned());
    ext.context_info = MessageField::some(context);
    let mut message = wa::Message::default();
    message.extended_text_message = MessageField::some(ext);
    message
}

pub(super) fn map(row: &crate::map::Row) -> Mapped {
    super::super::map_row(row, &ctx()).expect("maps")
}

/// The message of a row that may hold more events before it (a merge); every event is valid.
pub(super) fn message_of(mapped: &Mapped) -> IncomingMessage {
    checked(&mapped.events);
    match mapped.events.last() {
        Some(BackendEvent::Message(m)) => m.clone(),
        other => panic!("the last event is not a message: {other:?}"),
    }
}

#[test]
fn a_text_has_its_formatting_read_and_a_mention_by_lid_and_one_by_number_are_one_placeholder_each()
{
    let body = "hi @99900100999 and @15550100102, this is *bold*";
    let message = said(body, |c| {
        c.mentioned_jid = vec![format!("{OWN_LID}@lid"), MEMBER_PN.to_owned()];
    });
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    let Body::Text(text) = &got.body else {
        panic!("a text: {:?}", got.body);
    };
    assert_eq!(
        text.body.matches('\u{FFFC}').count(),
        2,
        "one placeholder each"
    );
    assert!(!text.body.contains('*'), "the bold markers are gone");
    let mentions: Vec<&UserId> = text
        .ranges
        .iter()
        .filter_map(|r| match &r.kind {
            RangeKind::Mention(user) => Some(user),
            _ => None,
        })
        .collect();
    assert_eq!(
        mentions,
        [&user("lid", OWN_LID), &user("pn", "15550100102")],
        "each person as the stanza names him"
    );
    assert!(text.ranges.iter().any(|r| r.kind == RangeKind::Bold));
    assert!(got.mentions_me, "our LID is among them");
}

#[test]
fn we_are_mentioned_by_either_of_our_ids_and_by_nobody_else() {
    let mention = |jid: &str| {
        let message = said("@someone hi", |c| c.mentioned_jid = vec![jid.to_owned()]);
        message_of(&map(&row_with(&message, &meta(), PEER, PEER))).mentions_me
    };
    assert!(mention(&format!("{OWN_LID}@lid")), "by our LID");
    assert!(
        mention(&format!("{OWN_PN}@s.whatsapp.net")),
        "by our number"
    );
    assert!(!mention(MEMBER_PN), "somebody else");
    assert!(!mention("12345@bot"), "no person");
    // A mention whose `@name` is not in the text is dropped from the text, but it still mentions us.
    let message = said("no name in here", |c| {
        c.mentioned_jid = vec![format!("{OWN_LID}@lid")];
    });
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    assert!(matches!(&got.body, Body::Text(t) if t.body == "no name in here"));
    assert!(got.mentions_me);
}

#[test]
fn a_quote_names_the_quoted_message_its_author_and_keeps_a_snapshot() {
    // A group: the quote's author is the participant, whoever wrote the quoting message.
    let mut quoted = wa::Message::default();
    quoted.conversation = Some("quoted *words*".into());
    let message = said("an answer", |c| {
        c.stanza_id = Some("3EB0QUOTED1".into());
        c.participant = Some(MEMBER_PN.into());
        c.quoted_message = MessageField::some(quoted);
    });
    let mut group = meta();
    group.is_group = true;
    let got = message_of(&map(&row_with(
        &message,
        &group,
        GROUP,
        "99900100101:3@lid",
    )));
    let reply = got.reply_to.expect("a quote");
    assert_eq!(reply.target.conv, Some(got.conv.clone()));
    assert_eq!(
        reply.target.conv,
        Some(ConvKey::Group("wag:120363000000000001".into()))
    );
    assert_eq!(
        reply.target.author,
        TargetAuthor::User(user("pn", "15550100102"))
    );
    assert_eq!(&*reply.target.local, "3EB0QUOTED1");
    assert_eq!(reply.author, Some(user("pn", "15550100102")));
    let snapshot = reply.snapshot.expect("the sender's copy");
    let text = snapshot.text.expect("its text");
    assert_eq!(
        text.body, "quoted words",
        "WhatsApp's formatting read in the snapshot too"
    );
    assert!(text.ranges.iter().any(|r| r.kind == RangeKind::Bold));
    assert_eq!(snapshot.attachment, None);

    // A direct chat without a participant: either side may have written it, so the author is open
    // (the core resolves by id inside the conversation). `remoteJid` is the chat as the SENDER sees
    // it, here our own number: it never becomes the conversation.
    let message = said("an answer", |c| {
        c.stanza_id = Some("3EB0QUOTED2".into());
        c.remote_jid = Some(format!("{OWN_PN}@s.whatsapp.net"));
    });
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    let reply = got.reply_to.expect("a quote");
    assert_eq!(
        reply.target.conv,
        Some(ConvKey::Direct(user("pn", "15550100101")))
    );
    assert_eq!(reply.target.author, TargetAuthor::Unknown);
    assert_eq!(reply.author, None);
    assert!(reply.snapshot.is_none(), "no copy, no snapshot");

    // Our own message quoted: the participant is one of our ids, by LID or by number.
    for own in [format!("{OWN_LID}@lid"), format!("{OWN_PN}@s.whatsapp.net")] {
        let message = said("an answer", |c| {
            c.stanza_id = Some("3EB0QUOTED3".into());
            c.participant = Some(own.clone());
        });
        let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
        let reply = got.reply_to.expect("a quote");
        assert_eq!(reply.target.author, TargetAuthor::Own, "{own}");
        assert_eq!(reply.author, None);
    }

    // The stanza names the person in the other of his ids than the quoted message was stored under:
    // as stated (the core's aliases join them once a stanza vouches for the pair).
    let message = said("an answer", |c| {
        c.stanza_id = Some("3EB0QUOTED4".into());
        c.participant = Some(MEMBER_LID.into());
    });
    let got = message_of(&map(&row_with(&message, &meta(), PEER_LID, PEER_LID)));
    let reply = got.reply_to.expect("a quote");
    assert_eq!(
        reply.target.author,
        TargetAuthor::User(user("lid", "99900100102"))
    );
}

#[test]
fn a_reply_to_a_status_keeps_the_snapshot_and_a_target_that_will_not_resolve() {
    let mut picture = wa::message::ImageMessage::default();
    picture.caption = Some("look".into());
    picture.mimetype = Some("image/jpeg".into());
    let mut quoted = wa::Message::default();
    quoted.image_message = MessageField::some(picture);
    let message = said("nice", |c| {
        c.stanza_id = Some("3EB0STATUS1".into());
        c.remote_jid = Some("status@broadcast".into());
        c.participant = Some(MEMBER_PN.into());
        c.quoted_message = MessageField::some(quoted);
    });
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    let reply = got.reply_to.expect("a quote");
    // The chat the reply was written in, not the feed: the status itself was let go, so nothing
    // there resolves.
    assert_eq!(reply.target.conv, Some(got.conv.clone()));
    assert_eq!(
        reply.target.author,
        TargetAuthor::User(user("pn", "15550100102"))
    );
    let snapshot = reply.snapshot.expect("the status as the sender saw it");
    assert_eq!(snapshot.text.expect("its caption").body, "look");
    assert_eq!(snapshot.attachment, Some(AttachmentKind::Image));
    assert_eq!(snapshot.mime.as_deref(), Some("image/jpeg"));

    // A quoted document shows its name.
    let mut document = wa::message::DocumentMessage::default();
    document.file_name = Some("report.pdf".into());
    let mut quoted = wa::Message::default();
    quoted.document_message = MessageField::some(document);
    let message = said("thanks", |c| {
        c.stanza_id = Some("3EB0DOC1".into());
        c.quoted_message = MessageField::some(quoted);
    });
    let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
    let snapshot = got.reply_to.expect("a quote").snapshot.expect("a snapshot");
    assert_eq!(snapshot.attachment, Some(AttachmentKind::File));
    assert_eq!(snapshot.file_name.as_deref(), Some("report.pdf"));
}

#[test]
fn a_quote_with_an_id_that_is_no_message_id_is_no_quote_and_the_message_still_maps() {
    for bad in ["", "bad\nid"] {
        let message = said("hello", |c| c.stanza_id = Some(bad.into()));
        let got = message_of(&map(&row_with(&message, &meta(), PEER, PEER)));
        assert!(got.reply_to.is_none(), "{bad:?}");
        assert!(matches!(&got.body, Body::Text(t) if t.body == "hello"));
    }
    let long = "x".repeat(2_000);
    let message = said("hello", |c| c.stanza_id = Some(long));
    assert!(
        message_of(&map(&row_with(&message, &meta(), PEER, PEER)))
            .reply_to
            .is_none()
    );
}

#[test]
fn a_row_whose_stanza_names_both_ids_merges_them_first() {
    let text = |body: &str| {
        let mut m = wa::Message::default();
        m.conversation = Some(body.to_owned());
        m
    };
    let merged = |mapped: &Mapped| -> Vec<(String, String)> {
        mapped
            .events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::IdentityMerged { from, into } => {
                    Some((from.as_str().to_owned(), into.as_str().to_owned()))
                }
                _ => None,
            })
            .collect()
    };

    // A sender by LID whose number the stanza gave: the number is the alias.
    let mut m = meta();
    m.sender_alt = Some(MEMBER_PN.into());
    let mapped = map(&row_with(&text("a"), &m, MEMBER_LID, MEMBER_LID));
    assert!(
        matches!(&mapped.events[0], BackendEvent::IdentityMerged { from, into }
            if from.as_str() == "pn:15550100102" && into.as_str() == "lid:99900100102"),
        "the merge is the FIRST event: {:?}",
        mapped.events
    );
    assert_eq!(mapped.events.len(), 2, "the merge and the message");
    checked(&mapped.events);

    // A sender by number whose LID the stanza gave: the same direction (number -> LID).
    let mut m = meta();
    m.sender_alt = Some(MEMBER_LID.into());
    let mapped = map(&row_with(&text("b"), &m, MEMBER_PN, MEMBER_PN));
    assert_eq!(
        merged(&mapped),
        [("pn:15550100102".to_owned(), "lid:99900100102".to_owned())]
    );

    // In a group, the member the stanza names twice.
    let mut m = meta();
    m.is_group = true;
    m.sender_alt = Some(MEMBER_PN.into());
    let mapped = map(&row_with(&text("c"), &m, GROUP, MEMBER_LID));
    assert_eq!(merged(&mapped).len(), 1);
    assert_eq!(
        message_of(&mapped).conv,
        ConvKey::Group("wag:120363000000000001".into())
    );

    // Nothing for our own ids (the core skips them too), nor for two ids of one kind, nor a broken one.
    let mut own = meta();
    own.from_me = true;
    own.sender_alt = Some(format!("{OWN_PN}@s.whatsapp.net"));
    let mapped = map(&row_with(
        &text("d"),
        &own,
        PEER_LID,
        &format!("{OWN_LID}:2@lid"),
    ));
    assert!(merged(&mapped).is_empty(), "our own pair");
    for alt in [MEMBER_LID, "not a jid@@", "120363000000000001@g.us"] {
        let mut m = meta();
        m.sender_alt = Some(alt.into());
        let mapped = map(&row_with(&text("e"), &m, MEMBER_LID, MEMBER_LID));
        assert!(merged(&mapped).is_empty(), "{alt}");
        assert_eq!(mapped.events.len(), 1, "the message maps all the same");
    }
    // No alternative identity, no merge.
    assert!(merged(&map(&row_with(&text("f"), &meta(), PEER, PEER))).is_empty());
}

#[test]
fn only_a_message_of_ours_vouches_a_pair_with_the_recipients_other_id() {
    let mut text = wa::Message::default();
    text.conversation = Some("to a LID chat".into());
    // Upstream sets `recipient_alt` for our own self-synced LID direct messages: the chat is the
    // peer's LID, the alternative is his number.
    let mut ours = meta();
    ours.from_me = true;
    ours.recipient_alt = Some(MEMBER_PN.into());
    let mapped = map(&row_with(
        &text,
        &ours,
        MEMBER_LID,
        &format!("{OWN_LID}:2@lid"),
    ));
    assert!(
        matches!(&mapped.events[0], BackendEvent::IdentityMerged { from, into }
            if from.as_str() == "pn:15550100102" && into.as_str() == "lid:99900100102"),
        "{:?}",
        mapped.events
    );
    assert!(matches!(
        &message_of(&mapped).origin,
        Sender::OwnOtherDevice { .. }
    ));

    // The same field on a message that is not ours is not a stanza's word about its sender.
    let mut theirs = meta();
    theirs.recipient_alt = Some(MEMBER_PN.into());
    let mapped = map(&row_with(&text, &theirs, MEMBER_LID, MEMBER_LID));
    assert_eq!(mapped.events.len(), 1, "no merge: {:?}", mapped.events);

    // Both fields naming the same pair give it once.
    let mut both = meta();
    both.from_me = true;
    both.sender_alt = Some(MEMBER_PN.into());
    both.recipient_alt = Some(MEMBER_PN.into());
    let mapped = map(&row_with(&text, &both, MEMBER_LID, MEMBER_LID));
    let merges = mapped
        .events
        .iter()
        .filter(|e| matches!(e, BackendEvent::IdentityMerged { .. }))
        .count();
    assert_eq!(merges, 1);
}

/// W6a2 B8a (D8): the map names the SENDER of a message of somebody else, cleaned; it never names us, the placeholder, a
/// blank or the sender of a row that made no event. Mutants: our own message named; "-" kept; a row with no event named.
#[test]
fn the_sender_of_a_shown_message_is_named_by_his_push_name_and_never_we_or_a_placeholder() {
    let named = |push: &str, from_me: bool, chat: &str, sender: &str, message: &wa::Message| {
        let mut m = meta();
        m.push_name = push.to_owned();
        m.from_me = from_me;
        map(&row_with(message, &m, chat, sender))
            .push_name
            .map(|sender| (sender.user, sender.name))
    };
    let hello = crate::map::tests::text("hello");
    // A peer's message: the peer, with the name trimmed.
    assert_eq!(
        named("  Ann Example ", false, PEER, PEER, &hello),
        Some((user("pn", "15550100101"), "Ann Example".to_owned()))
    );
    // A group message: the member who wrote it, in the form the stanza names him.
    assert_eq!(
        named("Bob Example", false, GROUP, MEMBER_LID, &hello),
        Some((user("lid", "99900100102"), "Bob Example".to_owned()))
    );
    // Not named: the placeholder, a blank, our own other device, one of our ids.
    assert_eq!(named("-", false, PEER, PEER, &hello), None);
    assert_eq!(named("   ", false, PEER, PEER, &hello), None);
    assert_eq!(named("My Own Name", true, PEER, PEER, &hello), None);
    assert_eq!(
        named(
            "My Own Name",
            false,
            GROUP,
            &format!("{OWN_PN}@s.whatsapp.net"),
            &hello
        ),
        None
    );
    // A row that made no event (a key share, nothing else in it) names nobody.
    let mut share = wa::Message::default();
    share.sender_key_distribution_message =
        MessageField::some(wa::message::SenderKeyDistributionMessage::default());
    assert_eq!(named("Ann Example", false, PEER, PEER, &share), None);
}
