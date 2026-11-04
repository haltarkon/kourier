//! The map beside the modifiers (W3b, chunk C5): a reaction, an edit, a revoke and the chat's timer setting.
//! Numbers and ids are the fictitious 555-01xx range.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    Body, ConvKey, MsgTarget, Origin as Sender, Patch, RangeKind, SystemEvent, TargetAuthor,
};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::content::{MEMBER_PN, OWN_PN, ctx, map, user};
use super::{GROUP, PEER, checked, meta, protocol, row_with};
use crate::inbox::InboxMeta;
use crate::map::{GroupFacts, Mapped};

const SERVER_SECONDS: i64 = 1_700_000_000;
const SERVER_MS: i64 = SERVER_SECONDS * 1000;

fn key(id: &str, from_me: bool, remote: Option<&str>, participant: Option<&str>) -> wa::MessageKey {
    let mut key = wa::MessageKey::default();
    key.id = Some(id.into());
    key.from_me = Some(from_me);
    key.remote_jid = remote.map(str::to_owned);
    key.participant = participant.map(str::to_owned);
    key
}

fn reaction(key: wa::MessageKey, text: &str) -> wa::Message {
    let mut m = wa::message::ReactionMessage::default();
    m.key = MessageField::some(key);
    m.text = Some(text.into());
    // The sender's own clock differs from the server's on purpose: the server's time rules (critic M3).
    m.sender_timestamp_ms = Some(SERVER_MS - 5_000);
    let mut message = wa::Message::default();
    message.reaction_message = MessageField::some(m);
    message
}

fn revoke(key: wa::MessageKey) -> wa::Message {
    protocol(|p| {
        p.r#type = Some(wa::message::protocol_message::Type::REVOKE);
        p.key = MessageField::some(key);
    })
}

fn edit(key: wa::MessageKey, edited: wa::Message) -> wa::Message {
    protocol(|p| {
        p.r#type = Some(wa::message::protocol_message::Type::MESSAGE_EDIT);
        p.key = MessageField::some(key);
        p.edited_message = MessageField::some(edited);
        p.timestamp_ms = Some(SERVER_MS - 9_000);
    })
}

fn from_me() -> InboxMeta {
    let mut m = meta();
    m.from_me = true;
    m
}

fn group() -> InboxMeta {
    let mut m = meta();
    m.is_group = true;
    m
}

/// The one event of a modifier row.
fn only(mapped: &Mapped) -> BackendEvent {
    checked(&mapped.events);
    assert!(!mapped.deferred, "{mapped:?}");
    assert_eq!(mapped.events.len(), 1, "{mapped:?}");
    mapped.events[0].clone()
}

/// The target of a reaction row, mapped for a sender in a chat.
fn aimed(
    message: &wa::Message,
    meta: &InboxMeta,
    chat: &str,
    sender: &str,
) -> (MsgTarget, Option<kourier_model::UserId>) {
    match only(&map(&row_with(message, meta, chat, sender))) {
        BackendEvent::Reaction { target, by, .. } | BackendEvent::Deleted { target, by, .. } => {
            (target, by)
        }
        other => panic!("not a reaction or a delete: {other:?}"),
    }
}

#[test]
fn a_reaction_and_its_removal_target_the_message_by_its_key() {
    let own = format!("{OWN_PN}@s.whatsapp.net");
    let added = reaction(key("3EB0TARGET1", false, Some(&own), None), "👍");
    let got = only(&map(&row_with(&added, &meta(), PEER, PEER)));
    let BackendEvent::Reaction {
        target,
        by,
        emoji,
        remove,
        at,
    } = got
    else {
        panic!("a reaction: {got:?}");
    };
    assert_eq!(
        target.conv,
        Some(ConvKey::Direct(user("pn", "15550100101")))
    );
    assert_eq!(&*target.local, "3EB0TARGET1");
    assert_eq!(
        target.author,
        TargetAuthor::Own,
        "the peer reacts to OUR message"
    );
    assert_eq!(by, Some(user("pn", "15550100101")));
    assert_eq!((emoji.as_str(), remove), ("👍", false));
    assert_eq!(at, SERVER_MS, "the server's time, not the sender's");

    let removed = reaction(key("3EB0TARGET1", false, Some(&own), None), "");
    let BackendEvent::Reaction { emoji, remove, .. } =
        only(&map(&row_with(&removed, &meta(), PEER, PEER)))
    else {
        panic!("a reaction");
    };
    assert!(
        remove && emoji.is_empty(),
        "an empty text takes the reaction back"
    );
}

/// Fix F3 (protocol review P1; the lead's decision, Q8.1): the removal of a reaction keeps ONE shape,
/// `Reaction { emoji: "", remove: true }`, targeting the same message by the same author and key as the
/// reaction it takes back. The CORE rule that a removal with no emoji takes away that sender's reaction on
/// the message (WhatsApp allows one per sender) is a main step; the map must not paper over it with an
/// invented emoji. The sender's own clock (here 5 s before the server's) never decides the time.
#[test]
fn a_reaction_removal_is_exactly_an_empty_emoji_with_remove_set_on_the_same_target() {
    let own = format!("{OWN_PN}@s.whatsapp.net");
    let aimed_at = || key("3EB0TARGET2", false, Some(&own), None);
    let added = only(&map(&row_with(
        &reaction(aimed_at(), "👍"),
        &meta(),
        PEER,
        PEER,
    )));
    let removed = only(&map(&row_with(
        &reaction(aimed_at(), ""),
        &meta(),
        PEER,
        PEER,
    )));
    let (
        BackendEvent::Reaction {
            target: first,
            by: first_by,
            ..
        },
        BackendEvent::Reaction {
            target,
            by,
            emoji,
            remove,
            at,
        },
    ) = (added, removed)
    else {
        panic!("two reactions");
    };
    assert_eq!(
        (emoji.as_str(), remove, at),
        ("", true, SERVER_MS),
        "the removal's shape"
    );
    assert_eq!(
        (target, by),
        (first, first_by),
        "the same target and sender"
    );
    // A reaction with no text at all (the field absent) is a removal too.
    let mut bare = reaction(aimed_at(), "x");
    if let Some(m) = bare.reaction_message.as_option().cloned() {
        let mut m = m;
        m.text = None;
        bare.reaction_message = MessageField::some(m);
    }
    let BackendEvent::Reaction { emoji, remove, .. } =
        only(&map(&row_with(&bare, &meta(), PEER, PEER)))
    else {
        panic!("a reaction");
    };
    assert!(remove && emoji.is_empty());
}

#[test]
fn the_author_of_a_target_in_a_1_to_1_chat_is_read_from_the_senders_side() {
    let peer = PEER;
    let own = format!("{OWN_PN}@s.whatsapp.net");
    let author =
        |message: &wa::Message, meta: &InboxMeta| aimed(message, meta, PEER, PEER).0.author;
    // 1. The peer on his own message: his key says fromMe, and remoteJid is us.
    assert_eq!(
        author(
            &reaction(key("3EB0T", true, Some(&own), None), "x"),
            &meta()
        ),
        TargetAuthor::User(user("pn", "15550100101")),
        "the peer on his own"
    );
    // 2. The peer on ours: not fromMe, and remoteJid is the chat as HE sees it, which is us.
    assert_eq!(
        author(
            &reaction(key("3EB0T", false, Some(&own), None), "x"),
            &meta()
        ),
        TargetAuthor::Own,
        "the peer on ours"
    );
    // 3. Our phone on our own message.
    assert_eq!(
        author(
            &reaction(key("3EB0T", true, Some(peer), None), "x"),
            &from_me()
        ),
        TargetAuthor::Own,
        "our phone on ours"
    );
    // 4. Our phone on the peer's: not fromMe, remoteJid is the peer.
    assert_eq!(
        author(
            &reaction(key("3EB0T", false, Some(peer), None), "x"),
            &from_me()
        ),
        TargetAuthor::User(user("pn", "15550100101")),
        "our phone on the peer's"
    );
    // Whatever remoteJid says, the target is in THIS row's conversation.
    let (target, by) = aimed(
        &reaction(key("3EB0T", false, Some(&own), None), "x"),
        &from_me(),
        PEER,
        PEER,
    );
    assert_eq!(
        target.conv,
        Some(ConvKey::Direct(user("pn", "15550100101")))
    );
    assert_eq!(by, None, "our own device reacts as the account");
}

#[test]
fn the_author_of_a_target_in_a_group_is_the_participant_the_key_names() {
    let sender = "99900100101:3@lid";
    let lid_of_sender = user("lid", "99900100101");
    let author = |key: wa::MessageKey, meta: &InboxMeta| {
        aimed(&reaction(key, "x"), meta, GROUP, sender).0.author
    };
    assert_eq!(
        author(key("3EB0T", false, Some(GROUP), Some(MEMBER_PN)), &group()),
        TargetAuthor::User(user("pn", "15550100102"))
    );
    assert_eq!(
        author(
            key(
                "3EB0T",
                false,
                Some(GROUP),
                Some(&format!("{OWN_PN}@s.whatsapp.net"))
            ),
            &group()
        ),
        TargetAuthor::Own,
        "a member reacts to OUR message"
    );
    assert_eq!(
        author(key("3EB0T", true, Some(GROUP), None), &group()),
        TargetAuthor::User(lid_of_sender),
        "fromMe in a group is the sender's own"
    );
    assert_eq!(
        author(key("3EB0T", false, Some(GROUP), None), &group()),
        TargetAuthor::Unknown,
        "none stated: the core finds it by its id inside the conversation"
    );
}

#[test]
fn a_reaction_or_revoke_that_names_no_valid_message_is_kept_not_mapped() {
    let mut no_id = key("x", false, Some(PEER), None);
    no_id.id = None;
    for message in [
        reaction(no_id.clone(), "👍"),
        reaction(key("has a\ncontrol", false, Some(PEER), None), "👍"),
        reaction(key("", false, Some(PEER), None), "👍"),
        revoke(no_id),
        reaction(wa::MessageKey::default(), "👍"),
    ] {
        let mapped = map(&row_with(&message, &meta(), PEER, PEER));
        assert!(mapped.deferred && mapped.events.is_empty(), "{mapped:?}");
    }
}

fn text_of(body: &Body) -> (&str, bool) {
    let Body::Text(t) = body else {
        panic!("a text: {body:?}");
    };
    (
        t.body.as_str(),
        t.ranges.iter().any(|r| r.kind == RangeKind::Bold),
    )
}

#[test]
fn an_edit_names_the_original_and_carries_its_own_id() {
    let own = format!("{OWN_PN}@s.whatsapp.net");
    let mut newer = wa::Message::default();
    newer.conversation = Some("the *new* words".into());
    let message = edit(key("3EB0ORIG1", false, Some(&own), None), newer);
    let mut row = row_with(&message, &meta(), PEER, PEER);
    row.id = "3EB0EDIT1".into();
    let BackendEvent::Edited {
        target,
        by,
        new_local,
        body,
        edited_at,
        keeps_quote,
    } = only(&map(&row))
    else {
        panic!("an edit");
    };
    assert_eq!(&*target.local, "3EB0ORIG1", "the ORIGINAL's id");
    assert_eq!(target.author, TargetAuthor::Own);
    assert_eq!(by, Some(user("pn", "15550100101")));
    assert_eq!(
        new_local.as_deref(),
        Some("3EB0EDIT1"),
        "the edit's own stanza id"
    );
    assert_eq!(
        text_of(&body),
        ("the new words", true),
        "WhatsApp's formatting read"
    );
    assert_eq!(
        edited_at, SERVER_MS,
        "the server's time, not the sender's timestampMs"
    );
    assert_eq!(keeps_quote, None);
}

#[test]
fn an_edit_of_a_caption_is_a_media_body_without_files_and_one_of_a_sticker_is_kept() {
    let own = format!("{OWN_PN}@s.whatsapp.net");
    let captioned = |caption: Option<&str>| {
        let mut image = wa::message::ImageMessage::default();
        image.caption = caption.map(str::to_owned);
        let mut edited = wa::Message::default();
        edited.image_message = MessageField::some(image);
        let message = edit(key("3EB0ORIG2", false, Some(&own), None), edited);
        let BackendEvent::Edited { body, .. } =
            only(&map(&row_with(&message, &meta(), PEER, PEER)))
        else {
            panic!("an edit");
        };
        body
    };
    let Body::Media {
        attachments,
        caption,
    } = captioned(Some("a *new* caption"))
    else {
        panic!("a media body");
    };
    assert!(attachments.is_empty(), "an edit never adds a file");
    assert_eq!(
        caption.as_ref().map(|c| c.body.as_str()),
        Some("a new caption")
    );
    assert!(
        matches!(captioned(None), Body::Media { caption: None, .. }),
        "no caption removes it"
    );
    // An edit of something that has no text or caption is kept, not guessed at.
    let mut sticker = wa::Message::default();
    sticker.sticker_message = MessageField::some(wa::message::StickerMessage::default());
    let message = edit(key("3EB0ORIG3", false, Some(&own), None), sticker);
    let mapped = map(&row_with(&message, &meta(), PEER, PEER));
    assert!(mapped.deferred && mapped.events.is_empty());
}

#[test]
fn a_revoke_by_the_author_is_a_delete_for_everyone_and_our_own_phones_is_the_accounts() {
    let sender = "99900100101:3@lid";
    // The author deletes his own message in a group.
    let own_message = revoke(key("3EB0GONE1", true, Some(GROUP), None));
    let BackendEvent::Deleted {
        target,
        by,
        for_everyone,
        at,
    } = only(&map(&row_with(&own_message, &group(), GROUP, sender)))
    else {
        panic!("a delete");
    };
    assert_eq!(
        target.author,
        TargetAuthor::User(user("lid", "99900100101"))
    );
    assert_eq!(by, Some(user("lid", "99900100101")));
    assert!(for_everyone);
    assert_eq!(at, SERVER_MS);
    // A key that names the sender himself as its participant is his own message too, whatever fromMe says.
    let named = revoke(key(
        "3EB0GONE4",
        false,
        Some(GROUP),
        Some("99900100101:3@lid"),
    ));
    let (target, by) = aimed(&named, &group(), GROUP, sender);
    assert_eq!(
        target.author,
        TargetAuthor::User(user("lid", "99900100101"))
    );
    assert_eq!(by, Some(user("lid", "99900100101")));
    // Our own phone's revoke of its own message is the account's.
    let (_, by) = aimed(
        &revoke(key("3EB0GONE3", true, Some(PEER), None)),
        &from_me(),
        PEER,
        PEER,
    );
    assert_eq!(by, None);
}

/// The map with the store's facts about a group's revokes.
fn map_with(row: &crate::map::Row, facts: &GroupFacts) -> Mapped {
    crate::map::map_row_with(row, &ctx(), Some(facts)).expect("maps")
}

fn group_key() -> ConvKey {
    ConvKey::Group("wag:120363000000000001".into())
}

/// An admin's delete-for-everyone in a group (W6a2, D11; the W3b fix F2 kept it deferred): the core applies it only when the
/// conversation states `admin_delete` AND its member list names the admin, and finishes it for good otherwise (the message
/// would stay). So the map says it needs the store's facts, then waits while the core was not told the group's member
/// list, and maps it to `Deleted{by: the sender}` once it was. Our own phone's admin delete is the account's (`by: None`).
/// Mutants: the delete is mapped before the member list is stated; the sender is taken for the deleter of the wrong id.
#[test]
fn an_admins_delete_in_a_group_waits_for_the_member_list_and_maps_after_it() {
    let admin = "99900100101:3@lid";
    let own_pn = format!("{OWN_PN}@s.whatsapp.net");
    let nothing = GroupFacts::default();
    let stated = GroupFacts::default().with_members_of(&group_key());
    let mut ours = group();
    ours.from_me = true;
    // Another member's message (his number as the participant), ours (our number) and one that names no author at all;
    // then our own phone deleting a member's message.
    let cases = [
        (
            key("3EB0GONE2", false, Some(GROUP), Some(MEMBER_PN)),
            group(),
            admin,
        ),
        (
            key("3EB0GONE5", false, Some(GROUP), Some(&own_pn)),
            group(),
            admin,
        ),
        (key("3EB0GONE6", false, Some(GROUP), None), group(), admin),
        (
            key("3EB0GONE8", false, Some(GROUP), Some(MEMBER_PN)),
            ours,
            "99900100999:2@lid",
        ),
    ];
    for (key, meta, sender) in cases {
        let row = row_with(&revoke(key), &meta, GROUP, sender);
        // Without the facts the map cannot tell this from the member's own delete in his other namespace.
        let asked = crate::map::map_row(&row, &ctx()).expect("maps");
        assert!(
            asked.needs_facts && asked.events.is_empty() && !asked.deferred,
            "{asked:?}"
        );
        // The member list was not stated: the row waits, with nothing said.
        let waits = map_with(&row, &nothing);
        assert_eq!(waits.waits_for_members, Some(group_key()), "{waits:?}");
        assert!(waits.events.is_empty() && !waits.deferred && !waits.needs_facts);
        // Stated: the delete maps, by the one who sent it.
        let BackendEvent::Deleted {
            by, for_everyone, ..
        } = only(&map_with(&row, &stated))
        else {
            panic!("a delete");
        };
        assert!(for_everyone);
        assert_eq!(by, (!meta.from_me).then(|| user("lid", "99900100101")));
    }
    // The target of a delete of another member's message is the participant the key names.
    let row = row_with(
        &revoke(key("3EB0GONE2", false, Some(GROUP), Some(MEMBER_PN))),
        &group(),
        GROUP,
        admin,
    );
    let BackendEvent::Deleted { target, .. } = only(&map_with(&row, &stated)) else {
        panic!("a delete");
    };
    assert_eq!(target.author, TargetAuthor::User(user("pn", "15550100102")));
    // A 1:1 chat has no admin: its revokes map as ever, with no facts.
    let mapped = map(&row_with(
        &revoke(key("3EB0GONE9", true, Some(PEER), None)),
        &meta(),
        PEER,
        PEER,
    ));
    assert!(!mapped.deferred && mapped.events.len() == 1, "{mapped:?}");
}

/// M3: a member deleting his OWN message, keyed by his number while he sends by his LID (or the other way round), is not an
/// admin's delete: it is mapped at once, whether or not the member list was stated, with the sender as author and deleter (the
/// core takes any other `by` for an admin's and would drop it). The pair must be one the store holds: a number that is not
/// his is an admin's delete. From our own phone, our other id is ours.
/// Mutants: the pair is not consulted (the delete waits); any pair is taken for the sender's.
#[test]
fn a_members_own_delete_in_his_other_namespace_is_not_an_admins() {
    let sender = "99900100101:3@lid";
    let none = GroupFacts::default();
    let paired =
        GroupFacts::default().with_pair(&user("pn", "15550100101"), &user("lid", "99900100101"));
    // His number as the participant: his own message under the other id.
    let row = row_with(
        &revoke(key("3EB0GONE7", false, Some(GROUP), Some(PEER))),
        &group(),
        GROUP,
        sender,
    );
    let BackendEvent::Deleted { target, by, .. } = only(&map_with(&row, &paired)) else {
        panic!("a delete");
    };
    assert_eq!(by, Some(user("lid", "99900100101")));
    assert_eq!(
        target.author,
        TargetAuthor::User(user("lid", "99900100101")),
        "the sender is the author, in the form he sends by"
    );
    // The same key with no such pair is somebody else's message: an admin's delete, waiting.
    let waits = map_with(&row, &none);
    assert_eq!(waits.waits_for_members, Some(group_key()), "{waits:?}");
    // A pair of ANOTHER number does not vouch for this one.
    let other =
        GroupFacts::default().with_pair(&user("pn", "15550100102"), &user("lid", "99900100101"));
    assert!(map_with(&row, &other).waits_for_members.is_some());
    // The other way round: he sends by his number and the key names his LID.
    let row = row_with(
        &revoke(key(
            "3EB0GONE7",
            false,
            Some(GROUP),
            Some("99900100101@lid"),
        )),
        &group(),
        GROUP,
        PEER,
    );
    let BackendEvent::Deleted { target, by, .. } = only(&map_with(&row, &paired)) else {
        panic!("a delete");
    };
    assert_eq!(by, Some(user("pn", "15550100101")));
    assert_eq!(target.author, TargetAuthor::User(user("pn", "15550100101")));
    // Our own phone deleting its own message keyed by our other id: the account's own delete.
    let mut ours = group();
    ours.from_me = true;
    let row = row_with(
        &revoke(key(
            "3EB0GONE8",
            false,
            Some(GROUP),
            Some(&format!("{OWN_PN}@s.whatsapp.net")),
        )),
        &ours,
        GROUP,
        "99900100999:2@lid",
    );
    let BackendEvent::Deleted { target, by, .. } = only(&map_with(&row, &none)) else {
        panic!("a delete");
    };
    assert_eq!(by, None);
    assert_eq!(target.author, TargetAuthor::Own);
}

fn timer_message(seconds: Option<u32>, version: i64) -> wa::Message {
    protocol(|p| {
        p.r#type = Some(wa::message::protocol_message::Type::EPHEMERAL_SETTING);
        p.ephemeral_expiration = seconds;
        p.ephemeral_setting_timestamp = Some(version);
    })
}

#[test]
fn a_timer_setting_sets_the_chats_timer_with_its_version_and_is_a_notice() {
    let mapped = map(&row_with(
        &timer_message(Some(86_400), 1_699_999_000),
        &meta(),
        PEER,
        PEER,
    ));
    checked(&mapped.events);
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ] = mapped.events.as_slice()
    else {
        panic!("an upsert, then a notice: {mapped:?}");
    };
    assert_eq!(patch.key, ConvKey::Direct(user("pn", "15550100101")));
    assert_eq!(patch.timer, Patch::Set(Duration::from_secs(86_400)));
    assert_eq!(patch.timer_version, Some(1_699_999_000));
    assert_eq!(
        notice.body,
        Body::System(SystemEvent::TimerChanged {
            by: Some(user("pn", "15550100101")),
            timer: Some(Duration::from_secs(86_400)),
        })
    );
    assert_eq!(&*notice.local, "3EB0AA01", "the stanza's id");
    assert!(
        matches!(notice.origin, Sender::Remote { .. }),
        "{:?}",
        notice.origin
    );
    assert_eq!(notice.sent_at, SERVER_MS);

    // Off: the timer is cleared and the notice says so.
    let off = map(&row_with(
        &timer_message(None, 1_699_999_500),
        &from_me(),
        PEER,
        PEER,
    ));
    let [
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ] = off.events.as_slice()
    else {
        panic!("an upsert, then a notice");
    };
    assert_eq!(patch.timer, Patch::Clear);
    assert_eq!(patch.timer_version, Some(1_699_999_500));
    assert_eq!(
        notice.body,
        Body::System(SystemEvent::TimerChanged {
            by: None,
            timer: None
        }),
        "our own phone's change"
    );
    // A group's change is announced by the group notice: the protocol message only sets the timer.
    let in_group = map(&row_with(
        &timer_message(Some(604_800), 1_699_999_000),
        &group(),
        GROUP,
        "99900100101:3@lid",
    ));
    checked(&in_group.events);
    assert!(
        matches!(in_group.events.as_slice(), [BackendEvent::ConversationUpsert(p)] if p.timer == Patch::Set(Duration::from_secs(604_800))),
        "{in_group:?}"
    );
}

#[test]
fn a_message_without_a_timer_still_has_none_after_a_timer_setting() {
    // The notice carries no timer, though the stanza the setting rode on came with the OLD one.
    let mut with_old = meta();
    with_old.ephemeral_expiration = Some(604_800);
    let setting = map(&row_with(
        &timer_message(Some(86_400), 1_699_999_000),
        &with_old,
        PEER,
        PEER,
    ));
    let Some(BackendEvent::Message(notice)) = setting.events.last() else {
        panic!("a notice");
    };
    assert!(notice.expiry.is_none(), "{:?}", notice.expiry);
    // A text that states no timer has none: the map holds no memory of the chat's setting (F1).
    let text = super::text("later, no timer");
    let later = map(&row_with(&text, &meta(), PEER, PEER));
    let Some(BackendEvent::Message(m)) = later.events.last() else {
        panic!("a message");
    };
    assert!(m.expiry.is_none());
}

#[test]
fn a_modifier_row_whose_stanza_names_both_ids_merges_them_first() {
    let own = format!("{OWN_PN}@s.whatsapp.net");
    let mut vouched = meta();
    vouched.sender_alt = Some("99900100101@lid".into());
    let message = reaction(key("3EB0TARGET9", false, Some(&own), None), "❤️");
    let mapped = map(&row_with(&message, &vouched, PEER, PEER));
    checked(&mapped.events);
    assert!(
        matches!(
            mapped.events.as_slice(),
            [
                BackendEvent::IdentityMerged { .. },
                BackendEvent::Reaction { .. }
            ]
        ),
        "{mapped:?}"
    );
}

/// W6a2 B8a (D8): a row the map has no event for yet (an admin's delete that needs the store's facts, or waits for the member
/// list) names nobody, so a person who made nothing visible is not stated; the same row names its sender once it maps.
/// Mutant: the sender named whatever the row came to.
#[test]
fn a_row_that_waits_or_needs_facts_names_nobody_until_it_maps() {
    let admin = "99900100101:3@lid";
    let mut meta = group();
    meta.push_name = "Ann Example".to_owned();
    let row = row_with(
        &revoke(key("3EB0GONE2", false, Some(GROUP), Some(MEMBER_PN))),
        &meta,
        GROUP,
        admin,
    );
    let asked = crate::map::map_row(&row, &ctx()).expect("maps");
    assert!(asked.needs_facts && asked.push_name.is_none());
    let waits = map_with(&row, &GroupFacts::default());
    assert!(waits.waits_for_members.is_some() && waits.push_name.is_none());
    let stated = GroupFacts::default().with_members_of(&group_key());
    let done = map_with(&row, &stated);
    let named = done.push_name.expect("named once it maps");
    assert_eq!(
        (named.user, named.name),
        (user("lid", "99900100101"), "Ann Example".to_owned())
    );
}
