//! Senders that do not play by the rules, against the ingest rules for modifiers (architecture
//! §3.5.1-3.5.3, §3.8, §4.9), on a Signal-like account (message ids are account-wide):
//!
//! - an edit, a delete or a reaction sent in one chat never lands on a message of another
//!   chat, direct or group, whichever arrives first, and the same modifier sent in the right
//!   chat still lands;
//! - a modifier sent under the other identity of a contact lands once the two chats are one;
//! - a delete for everyone that our own phone sent as a group admin is applied when we are an
//!   admin, and only then;
//! - an edit changes a text or a caption and nothing else (not a sticker, a contact card, a
//!   notice, view-once content, nor a voice note where the chat's files take no caption);
//! - receive windows are measured from the server's time for the message, not from the
//!   sender's clock;
//! - a reaction longer than any emoji is not stored.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::scripts::{T0, own, remote, signal_features, target};
use kourier_model::{
    Attachment, AttachmentKind, Body, ContactCard, ConvKey, ConversationPatch, FileLimits, Member,
    MemberList, MemberRole, MsgIdScope, NetworkKind, Patch, Placeholder, SystemEvent, Text, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
/// Somebody we have a direct chat with who is in none of our groups.
const MALLORY: &str = "aci:00000000-0000-4000-8000-00000000bad0";
const CAROL: &str = "aci:00000000-0000-4000-8000-0000000ca202";
const CAROL_PNI: &str = "pni:00000000-0000-4000-8000-0000000ca201";
const GROUP: &str = "gv2:aG9zdGlsZS10ZXN0LWdyb3Vw";

const HOUR: i64 = 3600 * 1000;
const DAY: i64 = 24 * HOUR;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn d(user: &str) -> ConvKey {
    ConvKey::Direct(uid(user))
}

fn group() -> ConvKey {
    ConvKey::Group(GROUP.into())
}

/// A fresh Signal-like account with direct chats with alice, bob and mallory and a group of
/// alice, bob and us, in which we have `my_role`.
fn harness(my_role: MemberRole) -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    h.set_now(T0);
    let chat = |key: ConvKey, members: Option<Vec<Member>>| {
        let mut p = ConversationPatch::new(key);
        p.features = Some(signal_features(members.is_some()));
        if members.is_some() {
            p.title = Patch::Set("Hostile Test".into());
        }
        p.members = members.map(MemberList::snapshot);
        BackendEvent::ConversationUpsert(p)
    };
    let member = |user: &str, role| Member {
        user: uid(user),
        role,
    };
    h.apply(vec![
        chat(d(ALICE), None),
        chat(d(BOB), None),
        chat(d(MALLORY), None),
        chat(
            group(),
            Some(vec![
                member(ALICE, MemberRole::Member),
                member(BOB, MemberRole::Member),
                member(ME, my_role),
            ]),
        ),
    ])
    .expect("chats");
    (dir, h)
}

/// What the tests look at of the message with network id `local`.
#[derive(Debug, PartialEq)]
struct Seen {
    body: Body,
    deleted: Option<String>,
    revision: i64,
    /// (user, emoji) of the reactions that are set.
    reactions: Vec<(String, String)>,
}

fn seen(h: &IngestHarness, local: &str) -> Seen {
    let c = h.connection();
    let (id, body, deleted, revision): (i64, String, Option<String>, i64) = c
        .query_row(
            "SELECT id, body, deleted, revision FROM messages WHERE net_local = ?1",
            [local],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("the message");
    let mut stmt = c
        .prepare(
            "SELECT user_id, emoji FROM reactions WHERE message_id = ?1 AND removed = 0
             ORDER BY user_id, emoji",
        )
        .expect("prepare");
    let reactions = stmt
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    Seen {
        body: serde_json::from_str(&body).expect("body"),
        deleted,
        revision,
        reactions,
    }
}

fn untouched(text: &str) -> Seen {
    Seen {
        body: Body::text(text),
        deleted: None,
        revision: 0,
        reactions: Vec::new(),
    }
}

fn count(h: &IngestHarness, sql: &str) -> i64 {
    h.connection()
        .query_row(sql, [], |r| r.get(0))
        .expect("count")
}

fn waiting(h: &IngestHarness) -> i64 {
    count(h, "SELECT count(*) FROM pending_modifiers")
}

fn edited(conv: &ConvKey, by: &str, local: &str, new_local: i64, text: &str) -> BackendEvent {
    BackendEvent::Edited {
        target: target(Some(conv), Some(&uid(by)), local),
        by: Some(uid(by)),
        new_local: Some(new_local.to_string().into()),
        body: Body::text(text),
        edited_at: new_local,
        keeps_quote: None,
    }
}

fn deleted(conv: &ConvKey, author: &str, by: Option<&str>, local: &str, at: i64) -> BackendEvent {
    BackendEvent::Deleted {
        target: target(Some(conv), Some(&uid(author)), local),
        by: by.map(uid),
        for_everyone: true,
        at,
    }
}

fn reacted(conv: &ConvKey, author: Option<&str>, by: &str, local: &str, at: i64) -> BackendEvent {
    BackendEvent::Reaction {
        target: target(Some(conv), author.map(uid).as_ref(), local),
        by: Some(uid(by)),
        emoji: "👍".into(),
        remove: false,
        at,
    }
}

/// One attack: a message, a modifier that names it from another chat, and the same modifier
/// as an honest client would send it (in the message's chat).
struct Case {
    what: &'static str,
    message: IncomingMessage,
    local: String,
    text: &'static str,
    /// Sent in a chat the message is not in.
    elsewhere: BackendEvent,
    /// Sent in the message's chat.
    here: BackendEvent,
    /// The message once `here` was applied.
    after: Seen,
}

fn cases() -> Vec<Case> {
    let in_group = (T0 + 1_000).to_string();
    let in_direct = (T0 + 2_000).to_string();
    let mine = (T0 + 3_000).to_string();
    let group_text = "said in the group";
    let direct_text = "said to us alone";
    let group_message = || remote(&group(), &uid(ALICE), &in_group, T0 + 1_000, group_text);
    let direct_message = || remote(&d(ALICE), &uid(ALICE), &in_direct, T0 + 2_000, direct_text);
    let reaction = |user: &str| vec![(user.to_owned(), "👍".to_owned())];
    let edit_done = |text: &str| Seen {
        body: Body::text(text),
        deleted: None,
        revision: 1,
        reactions: Vec::new(),
    };
    let delete_done = || Seen {
        body: Body::Text(Text::default()),
        deleted: Some("ForEveryone".into()),
        revision: 0,
        reactions: Vec::new(),
    };
    vec![
        // A group message, attacked through a direct chat. Its author can show us alone
        // another text, or take it away from us alone, while the group sees the original;
        // somebody who is not in the group can react to it.
        Case {
            what: "an edit of a group message sent in a direct chat",
            message: group_message(),
            local: in_group.clone(),
            text: group_text,
            elsewhere: edited(
                &d(ALICE),
                ALICE,
                &in_group,
                T0 + 5_000,
                "for your eyes only",
            ),
            here: edited(&group(), ALICE, &in_group, T0 + 6_000, "for the group"),
            after: edit_done("for the group"),
        },
        Case {
            what: "a delete of a group message sent in a direct chat",
            message: group_message(),
            local: in_group.clone(),
            text: group_text,
            elsewhere: deleted(&d(ALICE), ALICE, Some(ALICE), &in_group, T0 + 5_000),
            here: deleted(&group(), ALICE, Some(ALICE), &in_group, T0 + 6_000),
            after: delete_done(),
        },
        Case {
            what: "a reaction to a group message by somebody outside the group",
            message: group_message(),
            local: in_group.clone(),
            text: group_text,
            elsewhere: reacted(&d(MALLORY), Some(ALICE), MALLORY, &in_group, T0 + 5_000),
            here: reacted(&group(), Some(ALICE), BOB, &in_group, T0 + 6_000),
            after: Seen {
                reactions: reaction(BOB),
                ..untouched(group_text)
            },
        },
        // A direct message, attacked through a group both are in.
        Case {
            what: "an edit of a direct message sent in a group",
            message: direct_message(),
            local: in_direct.clone(),
            text: direct_text,
            elsewhere: edited(&group(), ALICE, &in_direct, T0 + 5_000, "said in the group"),
            here: edited(
                &d(ALICE),
                ALICE,
                &in_direct,
                T0 + 6_000,
                "said to us, reworded",
            ),
            after: edit_done("said to us, reworded"),
        },
        Case {
            what: "a delete of a direct message sent in a group",
            message: direct_message(),
            local: in_direct.clone(),
            text: direct_text,
            elsewhere: deleted(&group(), ALICE, Some(ALICE), &in_direct, T0 + 5_000),
            here: deleted(&d(ALICE), ALICE, Some(ALICE), &in_direct, T0 + 6_000),
            after: delete_done(),
        },
        Case {
            what: "a reaction to a direct message by a third person, sent in a group",
            message: direct_message(),
            local: in_direct.clone(),
            text: direct_text,
            elsewhere: reacted(&group(), Some(ALICE), BOB, &in_direct, T0 + 5_000),
            here: reacted(&d(ALICE), Some(ALICE), ALICE, &in_direct, T0 + 6_000),
            after: Seen {
                reactions: reaction(ALICE),
                ..untouched(direct_text)
            },
        },
        // Our own message to alice, named by somebody else in their own chat with us.
        Case {
            what: "a reaction to our own message in another direct chat",
            message: own(&d(ALICE), &mine, T0 + 3_000, "for alice"),
            local: mine.clone(),
            text: "for alice",
            elsewhere: reacted(&d(MALLORY), None, MALLORY, &mine, T0 + 5_000),
            here: reacted(&d(ALICE), None, ALICE, &mine, T0 + 6_000),
            after: Seen {
                reactions: reaction(ALICE),
                ..untouched("for alice")
            },
        },
    ]
}

/// The bug of phase G's review: under account-wide ids a modifier found its target by author
/// and id alone. Every case is played in both arrival orders. The modifier from the wrong
/// chat changes nothing, whether it finds the message there already or waits for it; both
/// orders end in the same state; and the modifier an honest client sends still lands.
#[test]
fn a_modifier_sent_in_another_chat_does_not_touch_the_message() {
    for case in cases() {
        let what = case.what;
        // The message is here first.
        let (_dir, mut first) = harness(MemberRole::Member);
        first
            .apply(vec![BackendEvent::Message(case.message.clone())])
            .expect("message");
        first.apply(vec![case.elsewhere.clone()]).expect("modifier");
        assert_eq!(seen(&first, &case.local), untouched(case.text), "{what}");
        assert_eq!(waiting(&first), 1, "{what}: it waits, it is not applied");

        // The modifier is here first and waits.
        let (_dir2, mut second) = harness(MemberRole::Member);
        second
            .apply(vec![case.elsewhere.clone()])
            .expect("modifier");
        assert_eq!(waiting(&second), 1, "{what}");
        second
            .apply(vec![BackendEvent::Message(case.message.clone())])
            .expect("message");
        assert_eq!(
            seen(&second, &case.local),
            untouched(case.text),
            "{what}, after waiting for the message"
        );
        assert_eq!(waiting(&second), 1, "{what}: still not applied");
        assert_eq!(
            first.dump(DumpMode::OrderInsensitive).expect("dump"),
            second.dump(DumpMode::OrderInsensitive).expect("dump"),
            "{what}: both orders end alike"
        );

        // A redelivery changes nothing either.
        first.apply(vec![case.elsewhere.clone()]).expect("again");
        assert_eq!(seen(&first, &case.local), untouched(case.text), "{what}");
        assert_eq!(waiting(&first), 1, "{what}: stored once");

        // The honest modifier lands, with the message here before or after it.
        first.apply(vec![case.here.clone()]).expect("honest");
        assert_eq!(seen(&first, &case.local), case.after, "{what}: in its chat");
        let (_dir3, mut third) = harness(MemberRole::Member);
        third
            .apply(vec![case.here.clone(), case.elsewhere.clone()])
            .expect("modifiers");
        assert_eq!(waiting(&third), 2, "{what}");
        third
            .apply(vec![BackendEvent::Message(case.message)])
            .expect("message");
        assert_eq!(
            seen(&third, &case.local),
            case.after,
            "{what}: the honest one waited and lands, the other does not"
        );
        assert_eq!(waiting(&third), 1, "{what}");

        // What never found its message goes when its time is up.
        let anything = || BackendEvent::ConversationUpsert(ConversationPatch::new(d(BOB)));
        third.set_now(T0 + 8 * DAY);
        third.apply(vec![anything()]).expect("purge");
        first.set_now(T0 + 8 * DAY);
        first.apply(vec![anything()]).expect("purge");
        assert_eq!((waiting(&first), waiting(&third)), (0, 0), "{what}");
    }
}

/// A delete for me and an attachment delete that our own phone synced name a conversation
/// too, and reach only a message of that conversation.
#[test]
fn deletes_synced_from_our_phone_stay_in_their_chat() {
    let local = (T0 + 1_000).to_string();
    let (_dir, mut h) = harness(MemberRole::Member);
    h.apply(vec![BackendEvent::Message(remote(
        &group(),
        &uid(ALICE),
        &local,
        T0 + 1_000,
        "kept",
    ))])
    .expect("message");
    let for_me = |conv: &ConvKey| BackendEvent::Deleted {
        target: target(Some(conv), Some(&uid(ALICE)), &local),
        by: None,
        for_everyone: false,
        at: T0 + 5_000,
    };
    h.apply(vec![for_me(&d(ALICE))]).expect("delete");
    assert_eq!(seen(&h, &local), untouched("kept"));
    h.apply(vec![for_me(&group())]).expect("delete");
    assert_eq!(seen(&h, &local).deleted.as_deref(), Some("ForMe"));
}

/// Carol is known by her phone number identity first. A reaction she sends from her account
/// identity names the chat under that identity, where our message is not (yet): it waits.
/// When the two identities turn out to be one person, the chats are one and it lands, in
/// every order of the three events.
#[test]
fn a_modifier_from_the_other_identity_lands_when_the_chats_merge() {
    let local = (T0 + 1_000).to_string();
    let message = BackendEvent::Message(own(&d(CAROL_PNI), &local, T0 + 1_000, "hello carol"));
    let reaction = reacted(&d(CAROL), None, CAROL, &local, T0 + 2_000);
    let merge = BackendEvent::IdentityMerged {
        from: uid(CAROL_PNI),
        into: uid(CAROL),
    };
    let orders: [[&BackendEvent; 3]; 6] = [
        [&message, &reaction, &merge],
        [&reaction, &message, &merge],
        [&message, &merge, &reaction],
        [&merge, &message, &reaction],
        [&merge, &reaction, &message],
        [&reaction, &merge, &message],
    ];
    let mut dumps = Vec::new();
    for order in orders {
        let (_dir, mut h) = harness(MemberRole::Member);
        for (step, event) in order.into_iter().enumerate() {
            h.apply(vec![event.clone()]).expect("apply");
            let merged = order[..=step].iter().any(|e| std::ptr::eq(*e, &merge));
            let stored = count(&h, "SELECT count(*) FROM messages") == 1;
            let has_reaction = order[..=step].iter().any(|e| std::ptr::eq(*e, &reaction));
            if stored && has_reaction && !merged {
                assert!(
                    seen(&h, &local).reactions.is_empty(),
                    "not before the chats are one"
                );
            }
        }
        assert_eq!(
            seen(&h, &local).reactions,
            [(CAROL.to_owned(), "👍".to_owned())]
        );
        assert_eq!(waiting(&h), 0);
        dumps.push(h.dump(DumpMode::OrderInsensitive).expect("dump"));
    }
    assert!(
        dumps.windows(2).all(|w| w[0] == w[1]),
        "every order ends alike"
    );
}

/// The second bug of phase G's review. We delete another member's message for everyone on
/// the phone, as a group admin; the phone syncs the delete (`by: None`). It is applied when
/// one of our ids is an admin of the group, in both arrival orders, and not when we are an
/// ordinary member or the network has no admin delete.
#[test]
fn an_admin_delete_synced_from_our_phone_is_applied() {
    let local = (T0 + 1_000).to_string();
    let message = || BackendEvent::Message(remote(&group(), &uid(BOB), &local, T0 + 1_000, "spam"));
    let synced = || deleted(&group(), BOB, None, &local, T0 + 5_000);
    let tombstone = |h: &IngestHarness| {
        let by: Option<String> = h
            .connection()
            .query_row(
                "SELECT deleted_by FROM messages WHERE net_local = ?1",
                [&local],
                |r| r.get(0),
            )
            .expect("row");
        (seen(h, &local).deleted, by)
    };

    // The message first, then the sync.
    let (_dir, mut h) = harness(MemberRole::Admin);
    h.apply(vec![message()]).expect("message");
    h.apply(vec![synced()]).expect("delete");
    assert_eq!(
        tombstone(&h),
        (Some("ForEveryone".into()), Some(String::new())),
        "deleted for everyone, by us"
    );
    assert_eq!(seen(&h, &local).body, Body::Text(Text::default()));

    // The sync first: it waits for the message.
    let (_dir2, mut early) = harness(MemberRole::Admin);
    early.apply(vec![synced()]).expect("delete");
    assert_eq!(waiting(&early), 1);
    early.apply(vec![message()]).expect("message");
    assert_eq!(tombstone(&early).0.as_deref(), Some("ForEveryone"));
    assert_eq!(
        h.dump(DumpMode::OrderInsensitive).expect("dump"),
        early.dump(DumpMode::OrderInsensitive).expect("dump")
    );

    // Not an admin: our account cannot have deleted it for everyone.
    let (_dir3, mut member) = harness(MemberRole::Member);
    member.apply(vec![message(), synced()]).expect("apply");
    assert_eq!(seen(&member, &local), untouched("spam"));

    // An admin, but the chat's features have no admin delete.
    let (_dir4, mut plain) = harness(MemberRole::Admin);
    let mut no_admin_delete = ConversationPatch::new(group());
    let mut features = signal_features(true);
    features.admin_delete = false;
    no_admin_delete.features = Some(features);
    plain
        .apply(vec![
            BackendEvent::ConversationUpsert(no_admin_delete),
            message(),
            synced(),
        ])
        .expect("apply");
    assert_eq!(seen(&plain, &local), untouched("spam"));

    // In a direct chat nobody is an admin: the other's message stays.
    let (_dir5, mut direct) = harness(MemberRole::Admin);
    direct
        .apply(vec![
            BackendEvent::Message(remote(&d(BOB), &uid(BOB), &local, T0 + 1_000, "spam")),
            deleted(&d(BOB), BOB, None, &local, T0 + 5_000),
        ])
        .expect("apply");
    assert_eq!(seen(&direct, &local), untouched("spam"));
}

fn voice() -> Attachment {
    Attachment {
        kind: AttachmentKind::Voice,
        mime: "audio/aac".into(),
        size: 4_000,
        ..Attachment::default()
    }
}

fn photo() -> Attachment {
    Attachment {
        kind: AttachmentKind::Image,
        mime: "image/jpeg".into(),
        size: 9_000,
        ..Attachment::default()
    }
}

/// An author's edit is a new text. It changes a text message or the caption of a message with
/// files; everything else stays what it is: a sticker, a contact card, a notice, view-once
/// content, a voice note where the chat's files carry no caption, and any message when the
/// edit itself is not a text.
#[test]
fn an_edit_changes_a_text_or_a_caption_and_nothing_else() {
    let (_dir, mut h) = harness(MemberRole::Member);
    // In this chat a voice note carries no caption (the network's rule, from its features).
    let mut features = signal_features(false);
    features.files.insert(
        AttachmentKind::Voice,
        FileLimits {
            max_size: 100 << 20,
            caption: false,
        },
    );
    let mut patch = ConversationPatch::new(d(ALICE));
    patch.features = Some(features);
    h.apply(vec![BackendEvent::ConversationUpsert(patch)])
        .expect("features");

    let at = |n: i64| T0 + n * 1_000;
    let local = |n: i64| at(n).to_string();
    let message = |n: i64, body: Body| {
        let mut m = remote(&d(ALICE), &uid(ALICE), &local(n), at(n), "");
        m.body = body;
        m
    };
    let media = |attachments: Vec<Attachment>, caption: Option<&str>| Body::Media {
        attachments,
        caption: caption.map(Text::plain),
    };
    let mut view_once = message(6, media(vec![photo()], None));
    view_once.view_once = true;
    let bodies: Vec<(i64, IncomingMessage)> = vec![
        (1, message(1, Body::Sticker(Box::new(photo())))),
        (
            2,
            message(
                2,
                Body::Contact(ContactCard {
                    name: Some("Dana".into()),
                    organization: None,
                    identifiers: Vec::new(),
                    avatar: None,
                }),
            ),
        ),
        (
            3,
            message(
                3,
                Body::System(SystemEvent::TitleChanged {
                    by: Some(uid(ALICE)),
                    title: "A new title".into(),
                }),
            ),
        ),
        (
            4,
            message(
                4,
                Body::Unsupported {
                    kind_hint: "view-once".into(),
                    summary: None,
                },
            ),
        ),
        (
            5,
            message(5, Body::Placeholder(Placeholder::ViewOnceOpened)),
        ),
        (6, view_once),
        (7, message(7, media(vec![voice()], None))),
    ];
    for (n, m) in &bodies {
        let before = m.body.clone();
        h.apply(vec![BackendEvent::Message(m.clone())])
            .expect("message");
        h.apply(vec![edited(
            &d(ALICE),
            ALICE,
            &local(*n),
            at(100 + n),
            "now it says this",
        )])
        .expect("edit");
        let got = seen(&h, &local(*n));
        assert_eq!(got.body, before, "message {n} is not a text");
        assert_eq!(got.revision, 0, "message {n}");
    }
    assert_eq!(
        count(&h, "SELECT count(*) FROM message_revisions"),
        0,
        "no revision was kept of an edit that did not apply"
    );
    assert_eq!(
        waiting(&h),
        0,
        "they were looked at and dropped, not parked"
    );

    // The same edits arriving before their messages are dropped all the same.
    let (_dir2, mut early) = harness(MemberRole::Member);
    let mut features = signal_features(false);
    features.files.insert(
        AttachmentKind::Voice,
        FileLimits {
            max_size: 100 << 20,
            caption: false,
        },
    );
    let mut patch = ConversationPatch::new(d(ALICE));
    patch.features = Some(features);
    early
        .apply(vec![BackendEvent::ConversationUpsert(patch)])
        .expect("features");
    for (n, m) in &bodies {
        early
            .apply(vec![edited(
                &d(ALICE),
                ALICE,
                &local(*n),
                at(100 + n),
                "now it says this",
            )])
            .expect("edit");
        early
            .apply(vec![BackendEvent::Message(m.clone())])
            .expect("message");
        assert_eq!(seen(&early, &local(*n)).body, m.body, "message {n}");
    }
    assert_eq!(count(&early, "SELECT count(*) FROM message_revisions"), 0);

    // What an edit does change: a text, the caption of a photo, the caption a voice note
    // already has (it can be removed, not replaced), and a message that could not be
    // decrypted.
    h.apply(vec![
        BackendEvent::Message(message(20, Body::text("a text"))),
        BackendEvent::Message(message(21, media(vec![photo()], Some("a caption")))),
        BackendEvent::Message(message(22, media(vec![voice()], Some("spoken")))),
        BackendEvent::Message(message(
            23,
            Body::Placeholder(Placeholder::Undecryptable { resendable: true }),
        )),
    ])
    .expect("messages");
    let edit_with = |n: i64, body: Body| BackendEvent::Edited {
        target: target(Some(&d(ALICE)), Some(&uid(ALICE)), &local(n)),
        by: Some(uid(ALICE)),
        new_local: Some(local(200 + n).into()),
        body,
        edited_at: at(200 + n),
        keeps_quote: None,
    };
    h.apply(vec![
        edit_with(20, Body::text("a better text")),
        // The edit lists other files: they are ignored, only its caption counts.
        edit_with(21, media(vec![voice(), voice()], Some("a better caption"))),
        edit_with(23, Body::text("readable after all")),
    ])
    .expect("edits");
    assert_eq!(seen(&h, &local(20)).body, Body::text("a better text"));
    assert_eq!(
        seen(&h, &local(21)).body,
        media(vec![photo()], Some("a better caption"))
    );
    assert_eq!(seen(&h, &local(23)).body, Body::text("readable after all"));
    // A new caption for the voice note is dropped; taking its caption away is not.
    h.apply(vec![edit_with(22, Body::text("rewritten"))])
        .expect("edit");
    assert_eq!(
        seen(&h, &local(22)).body,
        media(vec![voice()], Some("spoken"))
    );
    h.apply(vec![edit_with(22, media(Vec::new(), None))])
        .expect("edit");
    assert_eq!(seen(&h, &local(22)).body, media(vec![voice()], None));

    // An edit that is not a text does not turn a text into something else.
    let card = Body::Contact(ContactCard {
        name: Some("Dana".into()),
        organization: None,
        identifiers: Vec::new(),
        avatar: None,
    });
    for (n, body) in [
        (300, card),
        (301, Body::Sticker(Box::new(photo()))),
        (
            302,
            Body::System(SystemEvent::TitleChanged {
                by: None,
                title: "x".into(),
            }),
        ),
    ] {
        h.apply(vec![BackendEvent::Edited {
            target: target(Some(&d(ALICE)), Some(&uid(ALICE)), &local(220)),
            by: Some(uid(ALICE)),
            new_local: Some(local(n).into()),
            body,
            edited_at: at(n),
            keeps_quote: None,
        }])
        .expect("edit");
    }
    assert_eq!(seen(&h, &local(20)).body, Body::text("a better text"));
    assert_eq!(seen(&h, &local(20)).revision, 1);
}

/// Signal Desktop measures the receive windows from the server's time for the original, and
/// so does the core where the network gave one. A sender whose clock is off, or who dates a
/// message as it likes, does not move the window: the edit and the delete of a message the
/// server took three days ago are too late although the sender dated it "now", and those of a
/// message the server took an hour ago are in time although the sender's clock says it is
/// days old.
#[test]
fn receive_windows_are_measured_from_the_servers_time() {
    let (_dir, mut h) = harness(MemberRole::Member);
    let now = T0 + 10 * DAY;
    h.set_now(now);
    // (local, the sender's time, the server's time, still inside the two-day windows)
    let messages = [
        ("dated-forward", now - HOUR, Some(now - 3 * DAY), false),
        ("clock-behind", now - 5 * DAY, Some(now - HOUR), true),
        ("no-server-time", now - HOUR, None, true),
        ("no-server-time-old", now - 3 * DAY, None, false),
    ];
    for (name, sent_at, server_at, in_time) in messages {
        for (kind, suffix) in [("edit", "1"), ("delete", "2")] {
            let local = format!("{name}-{suffix}");
            let mut m = remote(&d(ALICE), &uid(ALICE), &local, sent_at, "original");
            m.server_at = server_at;
            h.apply(vec![BackendEvent::Message(m)]).expect("message");
            let modifier = if kind == "edit" {
                BackendEvent::Edited {
                    target: target(Some(&d(ALICE)), Some(&uid(ALICE)), &local),
                    by: Some(uid(ALICE)),
                    new_local: Some(format!("{local}-edit").into()),
                    body: Body::text("changed"),
                    edited_at: now,
                    keeps_quote: None,
                }
            } else {
                deleted(&d(ALICE), ALICE, Some(ALICE), &local, now)
            };
            h.apply(vec![modifier]).expect("modifier");
            let got = seen(&h, &local);
            let changed = got != untouched("original");
            assert_eq!(changed, in_time, "{kind} of {name}: {got:?}");
        }
    }
}

/// A reaction is one emoji. A peer that sends a long text as one does not get it stored
/// with the message (the reaction rows and chips would carry whatever it likes); the longest
/// real emoji passes.
#[test]
fn a_reaction_longer_than_any_emoji_is_dropped() {
    let local = (T0 + 1_000).to_string();
    let (_dir, mut h) = harness(MemberRole::Member);
    h.apply(vec![BackendEvent::Message(remote(
        &d(ALICE),
        &uid(ALICE),
        &local,
        T0 + 1_000,
        "hello",
    ))])
    .expect("message");
    let react = |emoji: &str, at: i64| BackendEvent::Reaction {
        target: target(Some(&d(ALICE)), Some(&uid(ALICE)), &local),
        by: Some(uid(ALICE)),
        emoji: emoji.into(),
        remove: false,
        at,
    };
    h.apply(vec![react(&"👍".repeat(17), T0 + 2_000)])
        .expect("reaction");
    assert!(
        seen(&h, &local).reactions.is_empty(),
        "68 bytes: not an emoji"
    );
    // Two people kissing, each with a skin tone: 35 bytes, among the longest there are.
    let kiss = "🧑🏻\u{200d}❤\u{fe0f}\u{200d}💋\u{200d}🧑🏿";
    h.apply(vec![react(kiss, T0 + 3_000)]).expect("reaction");
    assert_eq!(
        seen(&h, &local).reactions,
        [(ALICE.to_owned(), kiss.to_owned())]
    );
}
