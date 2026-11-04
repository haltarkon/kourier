//! What a send says (W5a, chunks 4 and 5). Synthetic ids only (the fictitious 555-01xx range and made-up LIDs,
//! each tail of 8 digits a substring of no other value); texts start with "synthetic".

use std::time::Duration;

use kourier_backend_api::{BackendEvent, OutgoingKind, OutgoingRequest};
use kourier_model::text::MENTION_PLACEHOLDER;
use kourier_model::{
    AttachmentKind, Body, ConvKey, MsgTarget, QuoteSnapshot, RangeKind, ReplyTo, TargetAuthor,
    Text, TextRange, UserId,
};
use wacore::proto_helpers::MessageExt as _;
use wacore_binary::Jid;
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use kourier_model::Attachment;

use super::{Built, Unbuildable, build, check_file, render};
use crate::map::{self, Ctx, Input, OwnIds, class};
use crate::markup;
use crate::media::{self, CAPTIONED_DOCUMENT, MediaPointer, hex};
use crate::send::address::{Addressing, Mapping, Namespace};

const PEER_LID: &str = "77700193852617";
const MEMBER_LID: &str = "77700148396025";
const OWN_LID: &str = "77700126473985";
const OWN_PN: &str = "15550137829461";
/// A member the core holds by number (fix F7).
const MEMBER_PN: &str = "15550184730926";
const GROUP: &str = "120363000057284913";
const ORIGINAL: &str = "3EB0A1B2C3D4E5F6071829";
const LOCAL: &str = "3EB09F8E7D6C5B4A392817";
const SENT_AT: i64 = 1_761_400_000_123;

/// No PN <-> LID pair is known.
struct NoPairs;

impl Mapping for NoPairs {
    fn lid_of(&self, _: &str) -> Option<String> {
        None
    }
    fn pn_of(&self, _: &str) -> Option<String> {
        None
    }
}

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a valid synthetic user")
}

fn own() -> OwnIds {
    OwnIds {
        lid: Some(user("lid", OWN_LID)),
        pn: Some(user("pn", OWN_PN)),
    }
}

fn dm() -> ConvKey {
    ConvKey::Direct(user("lid", PEER_LID))
}

fn group() -> ConvKey {
    ConvKey::Group(format!("wag:{GROUP}").into())
}

fn target(conv: &ConvKey, author: TargetAuthor) -> MsgTarget {
    MsgTarget {
        conv: Some(conv.clone()),
        author,
        local: ORIGINAL.into(),
        alternates: Vec::new(),
    }
}

fn req(conv: &ConvKey, kind: OutgoingKind) -> OutgoingRequest {
    OutgoingRequest {
        conv: conv.clone(),
        local: LOCAL.into(),
        sent_at: SENT_AT,
        kind,
        only: None,
        is_recipient_update: false,
    }
}

fn message(body: Body) -> OutgoingKind {
    OutgoingKind::Message {
        body,
        reply_to: None,
        expiry: None,
    }
}

/// Builds `kind` in `conv` for a migrated account, a group in LID mode.
fn built(conv: &ConvKey, kind: OutgoingKind) -> Result<Built, Unbuildable> {
    let own = own();
    let at = Addressing {
        migrated: true,
        map: &NoPairs,
        own: &own,
    };
    build(
        &req(conv, kind),
        &at,
        Some(Namespace::Lid),
        None,
        false,
        None,
    )
}

fn ranged(body: &str, ranges: &[(usize, usize, RangeKind)]) -> Text {
    Text {
        body: body.to_owned(),
        ranges: ranges
            .iter()
            .map(|(start, len, kind)| TextRange {
                start: u32::try_from(*start).expect("small"),
                len: u32::try_from(*len).expect("small"),
                kind: kind.clone(),
            })
            .collect(),
    }
}

/// The ranges of a text in one order (the parser writes inner pairs first).
fn sorted(text: &Text) -> Vec<(u32, u32, String)> {
    let mut ranges: Vec<(u32, u32, String)> = text
        .ranges
        .iter()
        .map(|r| (r.start, r.len, format!("{:?}", r.kind)))
        .collect();
    ranges.sort();
    ranges
}

fn same_text(a: &Text, b: &Text) -> bool {
    a.body == b.body && sorted(a) == sorted(b)
}

fn lid(value: &str) -> Option<Jid> {
    Some(Jid::new(value, wacore_binary::Server::Lid))
}

/// A mentioned person written as his LID.
fn as_lid(user: &UserId) -> Option<Jid> {
    lid(user.value())
}

// ---- chunk 4: the markup, the text message ------------------------------------------------------------------

#[test]
fn markup_renders_what_the_receive_map_parses() {
    let peer = user("lid", PEER_LID);
    let mention = format!("synthetic {MENTION_PLACEHOLDER} hi");
    let cases: Vec<(&str, Text, &str)> = vec![
        (
            "bold",
            ranged("synthetic bold text", &[(10, 4, RangeKind::Bold)]),
            "synthetic *bold* text",
        ),
        (
            "the four kinds",
            ranged(
                "synthetic a b c d",
                &[
                    (10, 1, RangeKind::Bold),
                    (12, 1, RangeKind::Italic),
                    (14, 1, RangeKind::Strike),
                    (16, 1, RangeKind::Mono),
                ],
            ),
            "synthetic *a* _b_ ~c~ `d`",
        ),
        (
            "nested",
            ranged(
                "synthetic one two",
                &[(10, 7, RangeKind::Bold), (14, 3, RangeKind::Italic)],
            ),
            "synthetic *one _two_*",
        ),
        (
            "crossing: closed and opened again",
            ranged(
                "synthetic ab cd ef",
                &[(10, 5, RangeKind::Bold), (13, 5, RangeKind::Italic)],
            ),
            "synthetic *ab _cd_* _ef_",
        ),
        (
            "mono innermost",
            ranged(
                "synthetic code word",
                &[(10, 9, RangeKind::Bold), (10, 4, RangeKind::Mono)],
            ),
            "synthetic *`code` word*",
        ),
        (
            "white space at the ends is left outside",
            ranged("synthetic  spaced  x", &[(10, 9, RangeKind::Strike)]),
            "synthetic  ~spaced~  x",
        ),
        (
            "a line break cuts a range",
            ranged("synthetic one\ntwo", &[(10, 7, RangeKind::Italic)]),
            "synthetic _one_\n_two_",
        ),
        (
            "a mention inside bold",
            ranged(
                &mention,
                &[
                    (10, 3, RangeKind::Mention(peer.clone())),
                    (10, 3, RangeKind::Bold),
                ],
            ),
            "synthetic *@77700193852617* hi",
        ),
    ];
    for (case, text, wire) in cases {
        let rendered = render(&text, &as_lid);
        assert_eq!(rendered.text, wire, "{case}: the wire text");
        assert!(!rendered.plain, "{case}: nothing was left out");
        let handles: Vec<markup::Mentioned> = rendered
            .mentioned
            .iter()
            .map(|jid| markup::Mentioned {
                handle: jid.trim_end_matches("@lid").to_owned(),
                user: user("lid", jid.trim_end_matches("@lid")),
                me: false,
            })
            .collect();
        let read = markup::mark_up(&rendered.text, &handles).text;
        // Formatting of one kind may come back in pieces: compare what each kind covers, white space aside
        // (a pair does not open before it or close after it).
        let cover = |t: &Text| {
            let body = t.body.clone();
            let mut bytes: Vec<(usize, String)> = t
                .ranges
                .iter()
                .flat_map(|r| {
                    (r.start..r.start + r.len).map(move |b| (b as usize, format!("{:?}", r.kind)))
                })
                .filter(|(b, _)| {
                    body.get(*b..)
                        .and_then(|rest| rest.chars().next())
                        .is_some_and(|c| !c.is_whitespace())
                })
                .collect();
            bytes.sort();
            bytes.dedup();
            bytes
        };
        assert_eq!(
            read.body, text.body,
            "{case}: the receiver reads the same text"
        );
        assert_eq!(
            cover(&read),
            cover(&text),
            "{case}: and the same formatting"
        );
    }
}

#[test]
fn formatting_the_markup_cannot_carry_is_left_out_and_the_text_is_whole() {
    // A range inside a word cannot be written; a marker typed in the text breaks the read-back.
    for (case, text, wire) in [
        (
            "inside a word",
            ranged("synthetic abcdef", &[(12, 2, RangeKind::Bold)]),
            "synthetic abcdef",
        ),
        (
            "a typed marker",
            ranged("synthetic a* b", &[(10, 4, RangeKind::Bold)]),
            "synthetic a* b",
        ),
    ] {
        let rendered = render(&text, &as_lid);
        assert_eq!(rendered.text, wire, "{case}");
        assert!(rendered.plain, "{case}: said to be plain");
    }
    // Spoilers and links have no markup: the text goes as it is, not counted as lost.
    let rendered = render(
        &ranged("synthetic spoiler", &[(10, 7, RangeKind::Spoiler)]),
        &as_lid,
    );
    assert_eq!(rendered.text, "synthetic spoiler");
    // A mention of everyone, or of a person with no id, leaves no placeholder on the wire.
    let rendered = render(
        &ranged(
            &format!("synthetic {MENTION_PLACEHOLDER}!"),
            &[(10, 3, RangeKind::MentionAll)],
        ),
        &as_lid,
    );
    assert_eq!(rendered.text, "synthetic !");
    assert!(rendered.mentioned.is_empty());
}

#[test]
fn a_text_is_a_conversation_unless_something_sits_beside_it() {
    // Plain: `conversation`, nothing else.
    let plain = built(&dm(), message(Body::text("synthetic hello"))).expect("built");
    assert_eq!(
        plain.message.conversation.as_deref(),
        Some("synthetic hello")
    );
    assert!(plain.message.extended_text_message.as_option().is_none());
    assert_eq!(plain.expiration, None);

    // A mention in a group: `extendedTextMessage` with the person's id in `mentionedJid`.
    let member = user("lid", MEMBER_LID);
    let text = ranged(
        &format!("synthetic {MENTION_PLACEHOLDER} look"),
        &[(10, 3, RangeKind::Mention(member))],
    );
    let mention = built(&group(), message(Body::Text(text))).expect("built");
    let m = mention
        .message
        .extended_text_message
        .as_option()
        .expect("an extended text");
    assert_eq!(m.text.as_deref(), Some("synthetic @77700148396025 look"));
    let info = m.context_info.as_option().expect("a context");
    assert_eq!(info.mentioned_jid, vec![format!("{MEMBER_LID}@lid")]);

    // The chat's timer goes to the send options; upstream writes it into the message.
    let timed = built(
        &dm(),
        OutgoingKind::Message {
            body: Body::text("synthetic timed"),
            reply_to: None,
            expiry: Some(Duration::from_secs(86_400)),
        },
    )
    .expect("built");
    assert_eq!(timed.expiration, Some(86_400));

    // What is nothing: a file message without a file.
    assert_eq!(
        built(
            &dm(),
            message(Body::Media {
                attachments: Vec::new(),
                caption: None
            })
        ),
        Err(Unbuildable::Empty)
    );
    assert_eq!(
        built(&dm(), message(Body::text("  "))),
        Err(Unbuildable::Empty)
    );
}

// ---- chunk 5: quotes, reactions, edits, revokes, and the way back -------------------------------------------

/// The events the receive map makes of `built` when it arrives from our phone (our own other device).
/// The `participant` of the key an edit, a revoke or a reaction carries.
fn key_participant(built: &Built) -> Option<String> {
    let message = &built.message;
    let key = message
        .protocol_message
        .as_option()
        .and_then(|p| p.key.as_option())
        .or_else(|| {
            message
                .reaction_message
                .as_option()
                .and_then(|r| r.key.as_option())
        })
        .expect("a message with a key");
    key.participant.clone()
}

fn mapped_back(conv: &ConvKey, built: &Built) -> BackendEvent {
    let mut message = built.message.clone();
    if let Some(seconds) = built.expiration {
        // What upstream does with `with_ephemeral_expiration`.
        assert!(message.set_ephemeral_expiration(seconds));
    }
    let base = class::peel(&message);
    let ctx = Ctx { own: own() };
    let mapped = map::message_events(
        Input {
            conv: conv.clone(),
            author: user("lid", OWN_LID),
            device: None,
            from_me: true,
            id: LOCAL,
            server_ms: SENT_AT + 900,
            received_at: SENT_AT + 1_000,
            message: &message,
            base,
            class: class::class_of(base),
            stored: None,
            carried: None,
            arrival: None,
            catch_up: false,
            merges: Vec::new(),
            facts: None,
        },
        &ctx,
    )
    .expect("the receive map takes it");
    assert!(!mapped.deferred, "the receive map defers nothing we send");
    mapped.events.into_iter().last().expect("one event")
}

/// A picture as the receive map stores it: an attachment with its remote pointer.
fn picture() -> kourier_model::Attachment {
    let mut m = wa::message::ImageMessage::default();
    m.mimetype = Some("image/jpeg".into());
    m.direct_path = Some("/v/t62.7118-24/synthetic".into());
    m.media_key = Some(vec![0xA5; 32]);
    m.file_sha256 = Some(vec![1; 32]);
    m.file_enc_sha256 = Some(vec![2; 32]);
    m.file_length = Some(54_321);
    m.width = Some(640);
    m.height = Some(480);
    m.caption = Some("synthetic old caption".into());
    let mut message = wa::Message::default();
    message.image_message = MessageField::some(m);
    crate::media::file_of(&message, class::Content::Image)
        .expect("a file")
        .attachment
}

#[test]
fn every_built_message_maps_back_to_the_same_target_and_body() {
    let peer = user("lid", PEER_LID);
    let member = user("lid", MEMBER_LID);

    // Texts: plain, formatted, a mention in a group, a timer.
    let formatted = ranged(
        "synthetic bold and mono",
        &[(10, 4, RangeKind::Bold), (19, 4, RangeKind::Mono)],
    );
    let mentioning = ranged(
        &format!("synthetic {MENTION_PLACEHOLDER} look"),
        &[(10, 3, RangeKind::Mention(member.clone()))],
    );
    for (case, conv, text, expiry) in [
        ("plain", dm(), Text::plain("synthetic plain"), None),
        ("markup", dm(), formatted, None),
        ("mention", group(), mentioning, None),
        (
            "expiry",
            dm(),
            Text::plain("synthetic timed"),
            Some(Duration::from_secs(604_800)),
        ),
    ] {
        let kind = OutgoingKind::Message {
            body: Body::Text(text.clone()),
            reply_to: None,
            expiry,
        };
        let BackendEvent::Message(event) = mapped_back(&conv, &built(&conv, kind).expect(case))
        else {
            panic!("{case}: a message comes back");
        };
        let Body::Text(back) = &event.body else {
            panic!("{case}: a text comes back");
        };
        assert!(same_text(back, &text), "{case}: the same text");
        assert_eq!(event.local.as_ref(), LOCAL, "{case}: under its id");
        assert_eq!(
            event.expiry.map(|e| e.timer),
            expiry,
            "{case}: the same timer"
        );
    }

    // Quotes: of our own message and of the peer's in a 1:1 chat, of a member's in a group.
    for (case, conv, author) in [
        ("own in a 1:1 chat", dm(), TargetAuthor::Own),
        ("the peer's", dm(), TargetAuthor::User(peer.clone())),
        ("a member's", group(), TargetAuthor::User(member.clone())),
        ("own in a group", group(), TargetAuthor::Own),
    ] {
        let quote = ReplyTo {
            target: target(&conv, author.clone()),
            author: None,
            snapshot: Some(QuoteSnapshot {
                text: Some(Text::plain("synthetic quoted")),
                attachment: Some(AttachmentKind::File),
                file_name: Some("synthetic.pdf".into()),
                mime: Some("application/pdf".into()),
                thumb: None,
            }),
        };
        let kind = OutgoingKind::Message {
            body: Body::text("synthetic answer"),
            reply_to: Some(quote),
            expiry: None,
        };
        let BackendEvent::Message(event) = mapped_back(&conv, &built(&conv, kind).expect(case))
        else {
            panic!("{case}: a message comes back");
        };
        let back = event.reply_to.expect("a quote comes back");
        assert_eq!(
            back.target,
            target(&conv, author),
            "{case}: the same target"
        );
        let snapshot = back.snapshot.expect("a snapshot");
        assert_eq!(
            snapshot.text.map(|t| t.body).as_deref(),
            Some("synthetic quoted"),
            "{case}"
        );
        assert_eq!(snapshot.attachment, Some(AttachmentKind::File), "{case}");
        assert_eq!(
            snapshot.file_name.as_deref(),
            Some("synthetic.pdf"),
            "{case}"
        );
    }

    // Reactions: added and taken back, to the peer's message and to our own.
    for (case, conv, author, remove) in [
        (
            "react to the peer's",
            dm(),
            TargetAuthor::User(peer.clone()),
            false,
        ),
        ("unreact to our own", dm(), TargetAuthor::Own, true),
        (
            "react to our own in a group",
            group(),
            TargetAuthor::Own,
            false,
        ),
        (
            "unreact to our own in a group",
            group(),
            TargetAuthor::Own,
            true,
        ),
        (
            "react to a member's",
            group(),
            TargetAuthor::User(member.clone()),
            false,
        ),
    ] {
        let kind = OutgoingKind::React {
            target: target(&conv, author.clone()),
            emoji: "\u{1F44D}".into(),
            remove,
        };
        let sent = built(&conv, kind).expect(case);
        if case.contains("our own in a group") {
            assert!(
                key_participant(&sent).is_some(),
                "{case}: the reaction's key names us (fix F10)"
            );
        }
        let BackendEvent::Reaction {
            target: back,
            by,
            remove: removed,
            emoji,
            ..
        } = mapped_back(&conv, &sent)
        else {
            panic!("{case}: a reaction comes back");
        };
        assert_eq!(back, target(&conv, author), "{case}: the same target");
        assert_eq!(by, None, "{case}: by us");
        assert_eq!(removed, remove, "{case}");
        if remove {
            assert!(emoji.is_empty(), "{case}: taken back with the empty text");
        } else {
            assert_eq!(emoji, "\u{1F44D}", "{case}");
        }
    }

    // Edits: a text and a caption, keyed by the ORIGINAL.
    for (case, conv) in [("edit in a 1:1 chat", dm()), ("edit in a group", group())] {
        let new = ranged("synthetic edited", &[(10, 6, RangeKind::Italic)]);
        let kind = OutgoingKind::Edit {
            target: target(&conv, TargetAuthor::Own),
            body: Body::Text(new.clone()),
        };
        let sent = built(&conv, kind).expect(case);
        // Fix F10: in a group the edit's key names us (the group's addressing mode), in a 1:1 chat nobody.
        assert_eq!(
            key_participant(&sent).is_some(),
            matches!(conv, ConvKey::Group(_)),
            "{case}: the participant of the edit's key"
        );
        let BackendEvent::Edited {
            target: back,
            new_local,
            body,
            ..
        } = mapped_back(&conv, &sent)
        else {
            panic!("{case}: an edit comes back");
        };
        assert_eq!(
            back,
            target(&conv, TargetAuthor::Own),
            "{case}: the original"
        );
        assert_eq!(
            new_local.as_deref(),
            Some(LOCAL),
            "{case}: the edit's own id"
        );
        let Body::Text(back) = body else {
            panic!("{case}: a text");
        };
        assert!(same_text(&back, &new), "{case}: the new text");
    }
    let caption = OutgoingKind::Edit {
        target: target(&dm(), TargetAuthor::Own),
        body: Body::Media {
            attachments: vec![picture()],
            caption: Some(Text::plain("synthetic new caption")),
        },
    };
    let caption = built(&dm(), caption).expect("a caption edit");
    let edited = caption
        .message
        .protocol_message
        .as_option()
        .and_then(|p| p.edited_message.as_option())
        .and_then(|m| m.image_message.as_option())
        .expect("the picture written again");
    assert_eq!(
        edited.direct_path.as_deref(),
        Some("/v/t62.7118-24/synthetic")
    );
    assert_eq!(edited.media_key.as_deref(), Some(&[0xA5; 32][..]));
    let BackendEvent::Edited {
        target: back, body, ..
    } = mapped_back(&dm(), &caption)
    else {
        panic!("a caption edit comes back");
    };
    assert_eq!(back, target(&dm(), TargetAuthor::Own));
    let Body::Media { caption, .. } = body else {
        panic!("a caption");
    };
    assert_eq!(
        caption.map(|c| c.body).as_deref(),
        Some("synthetic new caption")
    );

    // Delete for everyone of our own message, in a 1:1 chat and in a group.
    for (case, conv) in [
        ("revoke in a 1:1 chat", dm()),
        ("revoke in a group", group()),
    ] {
        let kind = OutgoingKind::Delete {
            target: target(&conv, TargetAuthor::Own),
            for_everyone: true,
        };
        let sent = built(&conv, kind).expect(case);
        assert_eq!(
            key_participant(&sent),
            None,
            "{case}: the sender revoke's key names nobody"
        );
        let BackendEvent::Deleted {
            target: back,
            by,
            for_everyone,
            ..
        } = mapped_back(&conv, &sent)
        else {
            panic!("{case}: a delete comes back");
        };
        assert_eq!(back, target(&conv, TargetAuthor::Own), "{case}");
        assert_eq!(by, None, "{case}");
        assert!(for_everyone, "{case}");
    }
}

#[test]
fn only_our_own_messages_are_edited_or_deleted_and_a_delete_for_me_is_not_sent() {
    let peer = TargetAuthor::User(user("lid", PEER_LID));
    assert_eq!(
        built(
            &dm(),
            OutgoingKind::Edit {
                target: target(&dm(), peer.clone()),
                body: Body::text("synthetic no"),
            }
        ),
        Err(Unbuildable::NotOwn)
    );
    assert_eq!(
        built(
            &dm(),
            OutgoingKind::Delete {
                target: target(&dm(), peer),
                for_everyone: true,
            }
        ),
        Err(Unbuildable::NotOwn)
    );
    assert_eq!(
        built(
            &dm(),
            OutgoingKind::Delete {
                target: target(&dm(), TargetAuthor::Own),
                for_everyone: false,
            }
        ),
        Err(Unbuildable::ForMe)
    );
    // A caption edit of a file without a pointer has nothing to write the file with.
    let mut bare = picture();
    bare.remote = None;
    assert_eq!(
        built(
            &dm(),
            OutgoingKind::Edit {
                target: target(&dm(), TargetAuthor::Own),
                body: Body::Media {
                    attachments: vec![bare],
                    caption: None,
                },
            }
        ),
        Err(Unbuildable::NoFile)
    );
    // The revoke key says the message is ours (a `fromMe = false` revoke is an admin's).
    let revoke = built(
        &group(),
        OutgoingKind::Delete {
            target: target(&group(), TargetAuthor::Own),
            for_everyone: true,
        },
    )
    .expect("built");
    let key = revoke
        .message
        .protocol_message
        .as_option()
        .and_then(|p| p.key.as_option())
        .expect("a key");
    assert_eq!(key.from_me, Some(true));
    assert_eq!(key.participant, None);
    assert_eq!(key.id.as_deref(), Some(ORIGINAL));
}

/// `kind` built in `conv` for a migrated account, the group's mode `mode`, with the admin fact `admin` (the group's row).
fn built_as(
    conv: &ConvKey,
    kind: OutgoingKind,
    map: &dyn Mapping,
    mode: Option<Namespace>,
    admin: bool,
) -> Result<Built, Unbuildable> {
    let own = own();
    let at = Addressing {
        migrated: true,
        map,
        own: &own,
    };
    build(&req(conv, kind), &at, mode, None, admin, None)
}

/// W6a2 B7b (his U4): another member's message is deleted for everyone only by an admin of a GROUP, with the key of
/// another's message (`fromMe = false`, the author as `participant` in the group's mode, which upstream's send turns into
/// `edit="8"`); not by a member who is no admin, not in a 1:1 chat whatever the fact says, and our own message keeps the
/// sender's revoke (`fromMe = true`, no participant) even for an admin. An author the core does not know has no key.
/// Mutants: the admin fact ignored; a 1:1 chat allowed; `fromMe = true` on an admin revoke; the author written in the
/// wrong form.
#[test]
fn an_admin_deletes_another_members_message_with_the_key_of_anothers_message_and_nobody_else_does()
{
    let delete = |conv: &ConvKey, author: TargetAuthor| OutgoingKind::Delete {
        target: target(conv, author),
        for_everyone: true,
    };
    let key_of = |built: &Built| {
        let protocol = built
            .message
            .protocol_message
            .as_option()
            .expect("protocol");
        assert_eq!(
            protocol.r#type,
            Some(wa::message::protocol_message::Type::Revoke)
        );
        protocol.key.as_option().expect("a key").clone()
    };
    let member = || TargetAuthor::User(user("lid", MEMBER_LID));
    let lid_group = Some(Namespace::Lid);

    // An admin of a LID group: the member's key, the group as the chat.
    let revoke = built_as(
        &group(),
        delete(&group(), member()),
        &MemberPair,
        lid_group,
        true,
    )
    .expect("an admin deletes a member's message");
    let key = key_of(&revoke);
    assert_eq!(key.from_me, Some(false), "the key of another's message");
    assert_eq!(
        key.participant.as_deref(),
        Some(format!("{MEMBER_LID}@lid").as_str()),
        "the author, as the group writes him"
    );
    assert_eq!(
        key.remote_jid.as_deref(),
        Some(format!("{GROUP}@g.us").as_str())
    );
    assert_eq!(key.id.as_deref(), Some(ORIGINAL));
    assert_eq!(revoke.expiration, None, "a revoke carries no timer");

    // The core holds the author by number: the LID group is written his LID, a group addressed by numbers his number.
    let by_number = TargetAuthor::User(user("pn", MEMBER_PN));
    let lid_written = built_as(
        &group(),
        delete(&group(), by_number.clone()),
        &MemberPair,
        lid_group,
        true,
    )
    .expect("built");
    assert_eq!(
        key_of(&lid_written).participant.as_deref(),
        Some(format!("{MEMBER_LID}@lid").as_str()),
        "a LID group never reads his number"
    );
    let number_written = built_as(
        &group(),
        delete(&group(), TargetAuthor::User(user("lid", MEMBER_LID))),
        &MemberPair,
        Some(Namespace::Pn),
        true,
    )
    .expect("built");
    assert_eq!(
        key_of(&number_written).participant.as_deref(),
        Some(format!("{MEMBER_PN}@s.whatsapp.net").as_str()),
        "a group addressed by numbers names the author by number"
    );

    // No admin, no revoke of another's message.
    assert_eq!(
        built_as(
            &group(),
            delete(&group(), member()),
            &MemberPair,
            lid_group,
            false
        ),
        Err(Unbuildable::NotOwn)
    );
    // A 1:1 chat has no admin: whatever the fact says.
    assert_eq!(
        built_as(
            &dm(),
            delete(&dm(), TargetAuthor::User(user("lid", PEER_LID))),
            &NoPairs,
            None,
            true
        ),
        Err(Unbuildable::NotOwn)
    );
    // Our own message stays the sender's revoke, also for an admin.
    let own_revoke = built_as(
        &group(),
        delete(&group(), TargetAuthor::Own),
        &NoPairs,
        lid_group,
        true,
    )
    .expect("built");
    let key = key_of(&own_revoke);
    assert_eq!((key.from_me, key.participant), (Some(true), None));
    // A message whose author the core does not know names no message in a group.
    assert_eq!(
        built_as(
            &group(),
            delete(&group(), TargetAuthor::Unknown),
            &NoPairs,
            lid_group,
            true
        ),
        Err(Unbuildable::Address(
            crate::send::address::Unaddressable::UnknownAuthor
        ))
    );
}

/// Fix F3 (PROTOCOL-6): when the form the message arrived under is known, the admin's revoke writes it as it is, in
/// whichever mode the group is in now (a LID group keeps a number the original key carried; a number group keeps a LID
/// even though the mapping knows the number); only an ADMIN revoke reads it (a reaction never, a non-admin gets nothing).
/// Mutant: the form ignored at the build.
#[test]
fn an_admin_revoke_writes_the_authors_form_as_his_message_arrived() {
    let own = own();
    let at = Addressing {
        migrated: true,
        map: &MemberPair,
        own: &own,
    };
    let delete = |conv: &ConvKey| OutgoingKind::Delete {
        target: target(conv, TargetAuthor::User(user("lid", MEMBER_LID))),
        for_everyone: true,
    };
    let participant = |built: Built| {
        built
            .message
            .protocol_message
            .as_option()
            .and_then(|p| p.key.as_option().map(|key| key.participant.clone()))
            .expect("a key")
    };
    let number = user("pn", MEMBER_PN);
    let lid = user("lid", MEMBER_LID);
    // Arrived by number, the group is addressed by LIDs now.
    let built = build(
        &req(&group(), delete(&group())),
        &at,
        Some(Namespace::Lid),
        None,
        true,
        Some(&number),
    )
    .expect("built");
    assert_eq!(
        participant(built).as_deref(),
        Some(format!("{MEMBER_PN}@s.whatsapp.net").as_str())
    );
    // Arrived by LID, the group is addressed by numbers now.
    let built = build(
        &req(&group(), delete(&group())),
        &at,
        Some(Namespace::Pn),
        None,
        true,
        Some(&lid),
    )
    .expect("built");
    assert_eq!(
        participant(built).as_deref(),
        Some(format!("{MEMBER_LID}@lid").as_str())
    );
    // Not an admin: the form changes nothing (and nothing is built).
    assert_eq!(
        build(
            &req(&group(), delete(&group())),
            &at,
            Some(Namespace::Lid),
            None,
            false,
            Some(&number),
        ),
        Err(Unbuildable::NotOwn)
    );
    // A reaction names the author in the group's mode whatever the form says.
    let react = OutgoingKind::React {
        target: target(&group(), TargetAuthor::User(lid.clone())),
        emoji: "+".into(),
        remove: false,
    };
    let built = build(
        &req(&group(), react),
        &at,
        Some(Namespace::Lid),
        None,
        true,
        Some(&number),
    )
    .expect("built");
    let key = built
        .message
        .reaction_message
        .as_option()
        .and_then(|r| r.key.as_option().cloned())
        .expect("a key");
    assert_eq!(
        key.participant.as_deref(),
        Some(format!("{MEMBER_LID}@lid").as_str())
    );
}

/// The mapping knows one pair: [`MEMBER_PN`] <-> [`MEMBER_LID`].
struct MemberPair;

impl Mapping for MemberPair {
    fn lid_of(&self, pn: &str) -> Option<String> {
        (pn == MEMBER_PN).then(|| MEMBER_LID.to_owned())
    }
    fn pn_of(&self, lid: &str) -> Option<String> {
        (lid == MEMBER_LID).then(|| MEMBER_PN.to_owned())
    }
}

/// `kind` built in `conv` with the pairs `map` for a migrated account, the group's mode `mode`.
fn built_with(
    conv: &ConvKey,
    kind: OutgoingKind,
    map: &dyn Mapping,
    mode: Option<Namespace>,
) -> Built {
    let own = own();
    let at = Addressing {
        migrated: true,
        map,
        own: &own,
    };
    build(&req(conv, kind), &at, mode, None, false, None).expect("built")
}

/// Fix F7 (privacy LOW): in a LID-addressed group, and in a group whose mode could not be read, a person the core holds
/// by NUMBER whose LID the mapping does not know is never written: the mention is left out (no `@number` in the text, no
/// `mentionedJid` entry) and a quote leaves the participant out (the receiver finds the message by its id); a person
/// with a known LID is written as the LID; a group addressed by numbers names numbers; a 1:1 chat is not affected.
/// Mutants: the rule removed (`public_person` writes what `person_in` does); the quote's participant not covered.
#[test]
fn a_number_is_never_written_into_a_group_that_hides_numbers() {
    let unmapped = user("pn", MEMBER_PN);
    let body = format!("synthetic look {MENTION_PLACEHOLDER} here");
    let mention = |who: &UserId| ranged(&body, &[(15, 3, RangeKind::Mention(who.clone()))]);
    let quote = |author: TargetAuthor| ReplyTo {
        target: target(&group(), author),
        author: None,
        snapshot: Some(QuoteSnapshot {
            text: Some(ranged(
                &format!("synthetic quoted {MENTION_PLACEHOLDER}"),
                &[(17, 3, RangeKind::Mention(unmapped.clone()))],
            )),
            attachment: None,
            file_name: None,
            mime: None,
            thumb: None,
        }),
    };
    let answer = |text: Text, reply_to: Option<ReplyTo>| OutgoingKind::Message {
        body: Body::Text(text),
        reply_to,
        expiry: None,
    };
    for mode in [Some(Namespace::Lid), None] {
        // The mention of an unmapped number: left out.
        let m = built_with(&group(), answer(mention(&unmapped), None), &NoPairs, mode);
        assert!(
            m.withheld,
            "{mode:?}: the mention was not reported as withheld"
        );
        assert!(
            !format!("{:?}", m.message).contains("84730926"),
            "{mode:?}: the number is in the message"
        );
        let info = m
            .message
            .extended_text_message
            .as_option()
            .and_then(|e| e.context_info.as_option());
        assert!(
            info.is_none_or(|i| i.mentioned_jid.is_empty()),
            "{mode:?}: a mentionedJid entry was written for the number"
        );
        // A quote of an unmapped author: no participant (and no number in the quoted text).
        let q = built_with(
            &group(),
            answer(
                Text::plain("synthetic answer"),
                Some(quote(TargetAuthor::User(unmapped.clone()))),
            ),
            &NoPairs,
            mode,
        );
        assert!(
            q.withheld,
            "{mode:?}: the quote was not reported as withheld"
        );
        assert!(
            !format!("{:?}", q.message).contains("84730926"),
            "{mode:?}: the number is in the quoting message"
        );
        let context = q
            .message
            .extended_text_message
            .as_option()
            .and_then(|e| e.context_info.as_option())
            .expect("a quote has a context");
        assert_eq!(
            context.participant, None,
            "{mode:?}: the participant stays out"
        );
        assert_eq!(context.stanza_id.as_deref(), Some(ORIGINAL));
    }
    // A person who is already a LID is written as the LID in both modes; a number with a known LID in a LID group.
    let jids = |built: &Built| {
        built
            .message
            .extended_text_message
            .as_option()
            .and_then(|e| e.context_info.as_option())
            .map(|i| i.mentioned_jid.clone())
            .unwrap_or_default()
    };
    for mode in [Some(Namespace::Lid), None] {
        let known = built_with(
            &group(),
            answer(mention(&user("lid", MEMBER_LID)), None),
            &MemberPair,
            mode,
        );
        assert!(!known.withheld, "{mode:?}: a LID is no number");
        assert_eq!(jids(&known), vec![format!("{MEMBER_LID}@lid")]);
    }
    let known = built_with(
        &group(),
        answer(mention(&unmapped), None),
        &MemberPair,
        Some(Namespace::Lid),
    );
    assert!(!known.withheld, "a mapped number is no number to withhold");
    assert_eq!(jids(&known), vec![format!("{MEMBER_LID}@lid")]);
    let mapped = built_with(
        &group(),
        answer(
            Text::plain("synthetic answer"),
            Some(quote(TargetAuthor::User(unmapped.clone()))),
        ),
        &MemberPair,
        Some(Namespace::Lid),
    );
    assert!(!mapped.withheld, "a mapped number is no number to withhold");
    let context = mapped
        .message
        .extended_text_message
        .as_option()
        .and_then(|e| e.context_info.as_option())
        .expect("a context");
    assert_eq!(
        context.participant.as_deref(),
        Some(format!("{MEMBER_LID}@lid").as_str()),
        "a person with a known LID is written as the LID"
    );
    // A group addressed by numbers names numbers; a 1:1 chat is not a group.
    let pn_group = built_with(
        &group(),
        answer(mention(&unmapped), None),
        &NoPairs,
        Some(Namespace::Pn),
    );
    assert!(!pn_group.withheld);
    let info = pn_group
        .message
        .extended_text_message
        .as_option()
        .and_then(|e| e.context_info.as_option())
        .expect("a mention has a context");
    assert_eq!(
        info.mentioned_jid,
        vec![format!("{MEMBER_PN}@s.whatsapp.net")]
    );
    let direct = built_with(
        &dm(),
        answer(
            Text::plain("synthetic answer"),
            Some(ReplyTo {
                target: target(&dm(), TargetAuthor::User(unmapped)),
                author: None,
                snapshot: None,
            }),
        ),
        &NoPairs,
        None,
    );
    assert!(!direct.withheld, "a 1:1 chat is not withheld");
}

// ---- W5b B10: file messages and the caption edit of a file Kourier sent ---------------------------------------

/// Builds `kind` with the pointer of a file (the upload's or the record's), as [`built`] does.
fn built_file(
    conv: &ConvKey,
    kind: OutgoingKind,
    file: Option<&MediaPointer>,
) -> Result<Built, Unbuildable> {
    let own = own();
    let at = Addressing {
        migrated: true,
        map: &NoPairs,
        own: &own,
    };
    build(
        &req(conv, kind),
        &at,
        Some(Namespace::Lid),
        file,
        false,
        None,
    )
}

/// The pointer of a picture Kourier sent (`Prepared::pointer`'s shape): the re-encode's mimetype and size.
fn sent_picture() -> MediaPointer {
    MediaPointer {
        media_type: "image".into(),
        direct_path: "/v/t62.7118-24/synthetic-sent-40918273".into(),
        media_key: hex(&[0x5A; 32]),
        file_sha256: Some(hex(&[3; 32])),
        file_enc_sha256: Some(hex(&[4; 32])),
        file_length: 61_234,
        media_key_timestamp: Some(1_761_400_000),
        thumb_jpeg: Some(hex(&[0xFF, 0xD8, 0xFF, 0xD9])),
        url: Some("https://synthetic.invalid/sent-picture-51920384".into()),
        mimetype: Some("image/jpeg".into()),
        width: Some(2048),
        height: Some(1536),
        shape: Some("image".into()),
        ..MediaPointer::default()
    }
}

/// A document Kourier sent, with or without its caption wrapper.
fn sent_document(captioned: bool) -> MediaPointer {
    MediaPointer {
        media_type: "document".into(),
        direct_path: "/v/t62.7119-24/synthetic-sent-62039184".into(),
        media_key: hex(&[0x6B; 32]),
        file_sha256: Some(hex(&[5; 32])),
        file_enc_sha256: Some(hex(&[6; 32])),
        file_length: 7_340,
        media_key_timestamp: Some(1_761_400_001),
        url: Some("https://synthetic.invalid/sent-document-73140295".into()),
        mimetype: Some("application/pdf".into()),
        shape: Some(
            if captioned {
                CAPTIONED_DOCUMENT
            } else {
                "document"
            }
            .into(),
        ),
        ..MediaPointer::default()
    }
}

/// The core's own attachment of a file it sent: no pointer (the core never stores an own one), its OWN mimetype
/// and size (a PNG before the re-encode).
fn own_file(kind: AttachmentKind, mime: &str, name: Option<&str>) -> Attachment {
    Attachment {
        kind,
        mime: mime.into(),
        size: 912_345,
        file_name: name.map(str::to_owned),
        dims: (kind == AttachmentKind::Image).then_some((4000, 3000)),
        ..Attachment::default()
    }
}

/// Takes the caption out of the file message `message` (a picture's, or a document's inside its wrapper).
fn take_caption(message: &mut wa::Message) -> Option<String> {
    if let Some(m) = message.image_message.as_option_mut() {
        return m.caption.take();
    }
    message
        .document_with_caption_message
        .as_option_mut()?
        .message
        .as_option_mut()?
        .document_message
        .as_option_mut()?
        .caption
        .take()
}

/// Which file message `message` is.
fn field_of(message: &wa::Message) -> &'static str {
    if message.image_message.as_option().is_some() {
        "image"
    } else if wrapped_document(message).is_some() {
        "document"
    } else {
        "other"
    }
}

/// The document inside the caption wrapper of `message`.
fn wrapped_document(message: &wa::Message) -> Option<&wa::message::DocumentMessage> {
    message
        .document_with_caption_message
        .as_option()?
        .message
        .as_option()?
        .document_message
        .as_option()
}

#[test]
fn a_file_message_carries_one_file_with_its_caption_quote_and_mentions() {
    let member = user("lid", MEMBER_LID);
    let caption = ranged(
        &format!("synthetic {MENTION_PLACEHOLDER} look"),
        &[(10, 3, RangeKind::Mention(member.clone()))],
    );
    let picture = OutgoingKind::Message {
        body: Body::Media {
            attachments: vec![own_file(AttachmentKind::Image, "image/png", None)],
            caption: Some(caption.clone()),
        },
        reply_to: Some(ReplyTo {
            target: target(&group(), TargetAuthor::User(member.clone())),
            author: None,
            snapshot: None,
        }),
        expiry: Some(Duration::from_secs(86_400)),
    };
    let sent = built_file(&group(), picture.clone(), Some(&sent_picture())).expect("a picture");
    assert_eq!(
        sent.expiration,
        Some(86_400),
        "the timer goes to the options"
    );
    let m = sent
        .message
        .image_message
        .as_option()
        .expect("an imageMessage");
    assert_eq!(m.caption.as_deref(), Some("synthetic @77700148396025 look"));
    // The pointer's own fields win over the core's (the re-encode's type and size).
    assert_eq!(m.mimetype.as_deref(), Some("image/jpeg"));
    assert_eq!((m.width, m.height), (Some(2048), Some(1536)));
    assert_eq!(
        m.url.as_deref(),
        Some("https://synthetic.invalid/sent-picture-51920384")
    );
    let info = m.context_info.as_option().expect("the file's own context");
    assert_eq!(info.mentioned_jid, vec![format!("{MEMBER_LID}@lid")]);
    assert_eq!(info.stanza_id.as_deref(), Some(ORIGINAL));
    assert_eq!(
        info.participant.as_deref(),
        Some(format!("{MEMBER_LID}@lid").as_str())
    );
    // Our phone reads it back as the same file, caption and quote.
    let BackendEvent::Message(event) = mapped_back(&group(), &sent) else {
        panic!("a message comes back");
    };
    let Body::Media {
        attachments,
        caption: back,
    } = &event.body
    else {
        panic!("a file comes back");
    };
    assert!(same_text(back.as_ref().expect("a caption"), &caption));
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].kind, AttachmentKind::Image);
    assert_eq!(attachments[0].dims, Some((2048, 1536)));
    assert_eq!(
        event.reply_to.as_ref().map(|r| r.target.local.as_ref()),
        Some(ORIGINAL)
    );

    // A document with a caption goes inside its wrapper, the quote and the mentions with it.
    let document = OutgoingKind::Message {
        body: Body::Media {
            attachments: vec![own_file(
                AttachmentKind::File,
                "application/pdf",
                Some("synthetic-report-83019274.pdf"),
            )],
            caption: Some(caption.clone()),
        },
        reply_to: Some(ReplyTo {
            target: target(&group(), TargetAuthor::Own),
            author: None,
            snapshot: None,
        }),
        expiry: None,
    };
    let sent = built_file(&group(), document, Some(&sent_document(true))).expect("a document");
    assert!(sent.message.document_message.as_option().is_none());
    let doc = wrapped_document(&sent.message).expect("the wrapper");
    assert_eq!(
        doc.caption.as_deref(),
        Some("synthetic @77700148396025 look")
    );
    assert_eq!(
        doc.file_name.as_deref(),
        Some("synthetic-report-83019274.pdf")
    );
    assert_eq!(doc.title.as_deref(), Some("synthetic-report-83019274.pdf"));
    let info = doc.context_info.as_option().expect("the inner context");
    assert_eq!(info.mentioned_jid, vec![format!("{MEMBER_LID}@lid")]);
    assert_eq!(info.stanza_id.as_deref(), Some(ORIGINAL));
    let BackendEvent::Message(event) = mapped_back(&group(), &sent) else {
        panic!("a message comes back");
    };
    let Body::Media { caption: back, .. } = &event.body else {
        panic!("a file comes back");
    };
    assert!(same_text(back.as_ref().expect("a caption"), &caption));

    // G10: one file per message; no pointer is no file.
    let two = message(Body::Media {
        attachments: vec![
            own_file(AttachmentKind::Image, "image/png", None),
            own_file(AttachmentKind::Image, "image/png", None),
        ],
        caption: None,
    });
    assert_eq!(
        built_file(&dm(), two, Some(&sent_picture())),
        Err(Unbuildable::ManyFiles)
    );
    // (In the chat the picture's quote is of: the quote is judged before the pointer is missed, fix F4.)
    assert_eq!(
        built_file(&group(), picture, None),
        Err(Unbuildable::NoFile)
    );
}

/// Fix F8 (interop review HIGH-1): the chat's timer reaches EVERY file shape, in the context the receiver reads: the
/// picture, the video, the audio and the plain document through upstream's `set_ephemeral_expiration` (`Built::expiration`),
/// the captioned document (a wrapper that call does not reach) inside its INNER document with `Built::expiration` left
/// `None` (no warning line, nothing asked of upstream); and our own phone's receive map reads the chat's timer back from
/// each. Mutant: the inner expiration not written.
#[test]
fn every_file_shape_carries_the_chats_timer_where_the_receiver_reads_it() {
    let timer = Duration::from_secs(86_400);
    let pointer = |media_type: &str, shape: &str, mime: &str| MediaPointer {
        media_type: media_type.into(),
        shape: Some(shape.into()),
        mimetype: Some(mime.into()),
        ..sent_document(false)
    };
    let cases = [
        (
            "picture",
            own_file(AttachmentKind::Image, "image/png", None),
            sent_picture(),
            true,
        ),
        (
            "video",
            own_file(AttachmentKind::Video, "video/mp4", None),
            pointer("video", "video", "video/mp4"),
            true,
        ),
        (
            "audio",
            own_file(AttachmentKind::Audio, "audio/ogg", None),
            pointer("audio", "audio", "audio/ogg; codecs=opus"),
            false,
        ),
        (
            "document",
            own_file(
                AttachmentKind::File,
                "application/pdf",
                Some("synthetic-report-83019274.pdf"),
            ),
            sent_document(false),
            false,
        ),
        (
            "captioned document",
            own_file(
                AttachmentKind::File,
                "application/pdf",
                Some("synthetic-report-83019274.pdf"),
            ),
            sent_document(true),
            true,
        ),
    ];
    for (case, attachment, pointer, captioned) in cases {
        let kind = OutgoingKind::Message {
            body: Body::Media {
                attachments: vec![attachment],
                caption: captioned.then(|| Text::plain("synthetic caption")),
            },
            reply_to: None,
            expiry: Some(timer),
        };
        let sent = built_file(&dm(), kind, Some(&pointer)).expect(case);
        let wrapped = sent
            .message
            .document_with_caption_message
            .as_option()
            .is_some();
        assert_eq!(wrapped, case == "captioned document", "{case}");
        assert_eq!(
            sent.expiration,
            (!wrapped).then_some(86_400),
            "{case}: upstream is asked for every shape but the wrapper"
        );
        // The frame as it goes out: upstream's call where it is asked.
        let mut frame = sent.message.clone();
        if let Some(seconds) = sent.expiration {
            assert!(frame.set_ephemeral_expiration(seconds), "{case}");
        }
        let carried = media::file_context(&mut frame)
            .and_then(|field| field.as_option())
            .and_then(|info| info.expiration);
        assert_eq!(
            carried,
            Some(86_400),
            "{case}: the timer in the receiver's context"
        );
        // Our own phone's receive map reads the chat's timer back.
        let BackendEvent::Message(event) = mapped_back(&dm(), &sent) else {
            panic!("{case}: a message comes back");
        };
        assert_eq!(event.expiry.map(|e| e.timer), Some(timer), "{case}");
    }
    // No timer, no expiration anywhere (a chat without one).
    let sent = built_file(
        &dm(),
        message(Body::Media {
            attachments: vec![own_file(
                AttachmentKind::File,
                "application/pdf",
                Some("synthetic-report-83019274.pdf"),
            )],
            caption: Some(Text::plain("synthetic caption")),
        }),
        Some(&sent_document(true)),
    )
    .expect("a captioned document");
    assert_eq!(sent.expiration, None);
    let mut frame = sent.message;
    let inner = media::file_context(&mut frame).and_then(|field| field.as_option());
    assert_eq!(inner.and_then(|info| info.expiration), None);
}

/// A document as a phone sends it: with a caption inside `documentWithCaptionMessage`, or bare.
fn phones_document(captioned: bool) -> wa::Message {
    let mut m = wa::message::DocumentMessage::default();
    m.mimetype = Some("application/pdf".into());
    m.file_name = Some("synthetic-report-83019274.pdf".into());
    m.direct_path = Some("/v/t62.7119-24/synthetic-phone-51802736".into());
    m.media_key = Some(vec![0xA6; 32]);
    m.file_sha256 = Some(vec![7; 32]);
    m.file_enc_sha256 = Some(vec![8; 32]);
    m.file_length = Some(7_340);
    m.caption = Some("synthetic old caption".into());
    let mut message = wa::Message::default();
    if captioned {
        let mut inner = wa::Message::default();
        inner.document_message = MessageField::some(m);
        let mut wrapper = wa::message::FutureProofMessage::default();
        wrapper.message = MessageField::some(inner);
        message.document_with_caption_message = MessageField::some(wrapper);
    } else {
        message.document_message = MessageField::some(m);
    }
    message
}

/// Fix F11 (interop LOW-3): a caption edit of a document the PHONE sent writes the shape the phone sent: the receive
/// map keeps `documentWithCaptionMessage` in the pointer, the edit writes the wrapper again (and a bare document
/// stays bare); a phone's file is still written as it was read (no title invented). Mutant: the shape not set on
/// receive.
#[test]
fn a_caption_edit_of_a_captioned_document_from_the_phone_keeps_the_wrapper() {
    for (case, captioned) in [("captioned", true), ("bare", false)] {
        let received = Built {
            message: phones_document(captioned),
            expiration: None,
            plain: false,
            withheld: false,
        };
        let BackendEvent::Message(event) = mapped_back(&dm(), &received) else {
            panic!("{case}: a message comes back");
        };
        let Body::Media { attachments, .. } = event.body else {
            panic!("{case}: a file comes back");
        };
        let edit = OutgoingKind::Edit {
            target: target(&dm(), TargetAuthor::Own),
            body: Body::Media {
                attachments,
                caption: Some(Text::plain("synthetic new caption")),
            },
        };
        let built = built_file(&dm(), edit, None).expect(case);
        let edited = built
            .message
            .protocol_message
            .as_option()
            .and_then(|p| p.edited_message.as_option())
            .expect("the file written again");
        let doc = if captioned {
            assert!(edited.document_message.as_option().is_none(), "{case}");
            wrapped_document(edited).expect("the wrapper again")
        } else {
            assert!(edited.document_with_caption_message.as_option().is_none());
            edited
                .document_message
                .as_option()
                .expect("a bare document")
        };
        assert_eq!(
            doc.caption.as_deref(),
            Some("synthetic new caption"),
            "{case}"
        );
        assert_eq!(
            doc.direct_path.as_deref(),
            Some("/v/t62.7119-24/synthetic-phone-51802736"),
            "{case}"
        );
        assert_eq!(
            doc.title, None,
            "{case}: a phone's file is written as it was read"
        );
    }
}

#[test]
fn a_caption_edit_of_an_own_file_writes_the_sent_file() {
    // Sent with a caption, edited after a restart: the pointer comes from the TARGET's upload record.
    for (case, attachment, pointer, field) in [
        (
            "picture",
            own_file(AttachmentKind::Image, "image/png", None),
            sent_picture(),
            "image",
        ),
        (
            "document",
            own_file(
                AttachmentKind::File,
                "application/pdf",
                Some("synthetic-report-83019274.pdf"),
            ),
            sent_document(true),
            "document",
        ),
    ] {
        let sent = built_file(
            &dm(),
            message(Body::Media {
                attachments: vec![attachment.clone()],
                caption: Some(Text::plain("synthetic old caption")),
            }),
            Some(&pointer),
        )
        .expect(case);
        let edit = OutgoingKind::Edit {
            target: target(&dm(), TargetAuthor::Own),
            body: Body::Media {
                attachments: vec![attachment],
                caption: Some(Text::plain("synthetic new caption")),
            },
        };
        let edited = built_file(&dm(), edit.clone(), Some(&pointer)).expect(case);
        let mut edited = edited
            .message
            .protocol_message
            .as_option()
            .and_then(|p| p.edited_message.as_option())
            .cloned()
            .expect("the file written again");
        // The edit equals the sent message but for the caption.
        let mut sent = sent.message;
        assert_eq!(
            take_caption(&mut sent).as_deref(),
            Some("synthetic old caption")
        );
        assert_eq!(
            take_caption(&mut edited).as_deref(),
            Some("synthetic new caption"),
            "{case}"
        );
        assert_eq!(field_of(&edited), field, "{case}");
        assert_eq!(edited, sent, "{case}: the sent file again");
        // Without the record (and no pointer on the attachment) there is no file to write.
        assert_eq!(
            built_file(&dm(), edit, None),
            Err(Unbuildable::NoFile),
            "{case}"
        );
    }
    // A file from the phone carries its pointer: the record is not needed (W5a).
    let phone = OutgoingKind::Edit {
        target: target(&dm(), TargetAuthor::Own),
        body: Body::Media {
            attachments: vec![picture()],
            caption: Some(Text::plain("synthetic new caption")),
        },
    };
    let edited = built_file(&dm(), phone, None).expect("a phone's picture");
    let image = edited
        .message
        .protocol_message
        .as_option()
        .and_then(|p| p.edited_message.as_option())
        .and_then(|m| m.image_message.as_option())
        .expect("the picture");
    assert_eq!(
        image.direct_path.as_deref(),
        Some("/v/t62.7118-24/synthetic")
    );
}

// ---- W5b fix F4: the refusals a pointer does not decide, before the file work -------------------------------

/// `check_file` of `kind` in `conv` (a LID group's mode, no pairs).
fn checked_file(conv: &ConvKey, kind: OutgoingKind) -> Result<super::FileCheck, Unbuildable> {
    let own = own();
    let at = Addressing {
        migrated: true,
        map: &NoPairs,
        own: &own,
    };
    check_file(&req(conv, kind), &at, Some(Namespace::Lid))
}

fn file_kind(
    attachments: Vec<Attachment>,
    caption: Option<Text>,
    quote: Option<ReplyTo>,
) -> OutgoingKind {
    OutgoingKind::Message {
        body: Body::Media {
            attachments,
            caption,
        },
        reply_to: quote,
        expiry: None,
    }
}

/// Fix F4: the refusals of a file message that do not depend on its pointer are found by the build's own function
/// before anything is read or uploaded: the same answers as `build` with a pointer in hand (a quote naming another
/// chat, no file, two files), and `captioned` is the fact of the RENDERED caption: a caption that is not blank by its
/// body but renders to nothing (a placeholder naming nobody) is none, and the built message then carries none.
/// Mutants: the quote not judged; `captioned` read from the body.
#[test]
fn the_checks_before_the_file_work_are_the_builds_own() {
    let one = || vec![own_file(AttachmentKind::Image, "image/png", None)];
    let elsewhere = ReplyTo {
        target: target(&dm(), TargetAuthor::Own),
        author: None,
        snapshot: None,
    };
    for (what, attachments, quote, refused) in [
        (
            "a quote of another chat",
            one(),
            Some(elsewhere.clone()),
            Unbuildable::Address(crate::send::address::Unaddressable::OtherConversation),
        ),
        ("no file", Vec::new(), None, Unbuildable::Empty),
        (
            "two files",
            vec![one().remove(0), one().remove(0)],
            None,
            Unbuildable::ManyFiles,
        ),
    ] {
        let kind = file_kind(attachments, Some(Text::plain("synthetic")), quote);
        assert_eq!(
            checked_file(&group(), kind.clone()).expect_err(what),
            refused,
            "{what}"
        );
        assert_eq!(
            built_file(&group(), kind, Some(&sent_picture())).expect_err(what),
            refused,
            "{what}: build says the same"
        );
    }

    // A clean file message is no refusal; its caption is the rendered one.
    let captioned = |caption: Option<Text>| checked_file(&group(), file_kind(one(), caption, None));
    assert_eq!(
        captioned(Some(Text::plain("synthetic caption"))),
        Ok(super::FileCheck { captioned: true })
    );
    assert_eq!(captioned(None), Ok(super::FileCheck { captioned: false }));
    assert_eq!(
        captioned(Some(Text::plain("  \n "))),
        Ok(super::FileCheck { captioned: false })
    );
    let nobody = Text::plain(MENTION_PLACEHOLDER.to_string());
    assert!(!nobody.body.trim().is_empty(), "not blank by its body");
    assert_eq!(
        captioned(Some(nobody.clone())),
        Ok(super::FileCheck { captioned: false }),
        "a placeholder naming nobody renders to nothing"
    );
    let mut sent = built_file(
        &group(),
        file_kind(one(), Some(nobody), None),
        Some(&sent_picture()),
    )
    .expect("built");
    assert_eq!(
        take_caption(&mut sent.message),
        None,
        "and the message has none"
    );
    // Anything that is not a new message with a file has nothing to check.
    assert_eq!(
        checked_file(
            &group(),
            OutgoingKind::Message {
                body: Body::text("synthetic hello"),
                reply_to: None,
                expiry: None,
            }
        ),
        Ok(super::FileCheck { captioned: false })
    );
}

/// W6a2 (B7b, D22, fix F1): the timer set is the protocol message of the setting (the timer in seconds, 0 = off, and the
/// time in seconds as its version) with the mode the pinned upstream writes (`CHANGED_IN_CHAT`, `UNKNOWN`: a copy of
/// upstream's `ephemeral_setting_message_shape`) and NOTHING else, and the receive map reads it back as the chat's timer
/// with that version and the notice. Mutants: no `ephemeralExpiration`; the time dropped; another protocol type; the
/// `disappearingMode` left out (F1).
#[test]
fn the_timer_setting_is_the_protocol_message_the_receive_map_reads_back() {
    use kourier_model::{Patch, SystemEvent};

    let at = 1_761_500_000;
    let message = super::timer_setting(604_800, at);
    let p = message
        .protocol_message
        .as_option()
        .expect("a protocol message");
    assert_eq!(
        p.r#type,
        Some(wa::message::protocol_message::Type::EphemeralSetting)
    );
    assert_eq!(p.ephemeral_expiration, Some(604_800));
    assert_eq!(p.ephemeral_setting_timestamp, Some(at));
    assert!(p.key.as_option().is_none(), "it names no message");
    let mode = p
        .disappearing_mode
        .as_option()
        .expect("the mode upstream writes");
    assert_eq!(
        mode.initiator,
        Some(wa::disappearing_mode::Initiator::ChangedInChat)
    );
    assert_eq!(mode.trigger, Some(wa::disappearing_mode::Trigger::Unknown));
    let mut rest = message.clone();
    rest.protocol_message = MessageField::none();
    assert_eq!(
        waproto::codec::message_encoded_len(&rest),
        0,
        "nothing beside it"
    );

    for (secs, timer) in [(604_800, Some(Duration::from_secs(604_800))), (0, None)] {
        let message = super::timer_setting(secs, at);
        let base = class::peel(&message);
        let mapped = map::message_events(
            Input {
                conv: dm(),
                author: user("lid", OWN_LID),
                device: None,
                from_me: true,
                id: LOCAL,
                server_ms: SENT_AT + 900,
                received_at: SENT_AT + 1_000,
                message: &message,
                base,
                class: class::class_of(base),
                stored: None,
                carried: None,
                arrival: None,
                catch_up: false,
                merges: Vec::new(),
                facts: None,
            },
            &Ctx { own: own() },
        )
        .expect("the receive map takes it");
        let [
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(notice),
        ] = mapped.events.as_slice()
        else {
            panic!("an upsert and a notice: {:?}", mapped.events);
        };
        assert_eq!(patch.timer, timer.map_or(Patch::Clear, Patch::Set));
        assert_eq!(
            patch.timer_version,
            u32::try_from(at).ok(),
            "the time is the version"
        );
        assert!(matches!(
            &notice.body,
            Body::System(SystemEvent::TimerChanged { timer: t, .. }) if *t == timer
        ));
    }
}
