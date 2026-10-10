//! The per-network script set of the conformance suite (§7.3, §10.2): one script per network
//! shape, each exercising what that network needs from the contract. All identities are
//! synthetic.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, HistoryEdit, HistoryReaction, IncomingMessage};
use kourier_model::{
    AccountSettingsPatch, AttachmentKind, Body, ContactPatch, ConvKey, ConversationFeatures,
    ConversationPatch, DisappearKind, FileAlbum, FileLimits, FilePacking, FormatKind,
    HistorySource, Member, MemberList, MemberRole, MsgTarget, NetMsgId, Origin, OutgoingState,
    Patch, Placeholder, QuoteSnapshot, RangeKind, ReceiptKind, RecipientState, ReplyTo, Support,
    SystemEvent, TargetAuthor, Text, TextRange, TimerRange, UserId, Window,
};

use crate::flavor::Flavor;
use crate::script::{LoginScript, Script, ScriptError, Step};

/// Base timestamp of the scripts (ms).
pub const T0: i64 = 1_760_000_000_000;

fn user(s: &str) -> Result<UserId, ScriptError> {
    Ok(UserId::parse(s)?)
}

/// A remote text message (`received_at` = `sent_at` + 500 ms).
pub fn remote(
    conv: &ConvKey,
    author: &UserId,
    local: &str,
    sent_at: i64,
    text: &str,
) -> IncomingMessage {
    IncomingMessage::new(
        conv.clone(),
        local,
        Origin::Remote {
            author: author.clone(),
            device: Some(1),
        },
        Body::text(text),
        sent_at,
        sent_at + 500,
    )
}

/// An own text message sent from another device.
pub fn own(conv: &ConvKey, local: &str, sent_at: i64, text: &str) -> IncomingMessage {
    IncomingMessage::new(
        conv.clone(),
        local,
        Origin::OwnOtherDevice { device: Some(1) },
        Body::text(text),
        sent_at,
        sent_at + 500,
    )
}

/// A reference to the message `(author, local)`.
pub fn target(conv: Option<&ConvKey>, author: Option<&UserId>, local: &str) -> MsgTarget {
    let id = NetMsgId {
        author: author.cloned(),
        local: Arc::from(local),
    };
    MsgTarget::to_id(conv.cloned(), &id)
}

/// A reference without an author (Threema).
pub fn unknown_target(conv: &ConvKey, local: &str) -> MsgTarget {
    MsgTarget {
        conv: Some(conv.clone()),
        author: TargetAuthor::Unknown,
        local: Arc::from(local),
        alternates: Vec::new(),
    }
}

fn contact(user: &UserId, profile: Option<&str>, system: Option<&str>) -> BackendEvent {
    let mut p = ContactPatch::new(user.clone());
    if let Some(n) = profile {
        p.profile_name = Patch::Set(n.into());
    }
    if let Some(n) = system {
        p.system_name = Patch::Set(n.into());
    }
    BackendEvent::ContactUpsert(p)
}

fn conversation(
    key: &ConvKey,
    title: Option<&str>,
    members: Option<Vec<Member>>,
    features: &ConversationFeatures,
) -> ConversationPatch {
    let mut p = ConversationPatch::new(key.clone());
    if let Some(t) = title {
        p.title = Patch::Set(t.into());
    }
    p.members = members.map(MemberList::snapshot);
    p.features = Some(features.clone());
    p
}

fn member(user: &UserId, role: MemberRole) -> Member {
    Member {
        user: user.clone(),
        role,
    }
}

fn day_window(days: u32) -> Option<Window> {
    Some(Window {
        max_age_secs: Some(days * 24 * 3600),
        max_count: None,
    })
}

/// How many pictures, GIFs and videos one message of the fake network carries
/// ([`file_packing`]). Small on purpose, so a handful of files proves the split.
pub const ALBUM_MAX: u16 = 3;

/// The fake network's packing rule: pictures, GIFs and videos share a message, up to
/// [`ALBUM_MAX`] of them; every other file travels alone; files can carry a quote. Not
/// Signal's values: the Signal backend states its own.
pub fn file_packing() -> FilePacking {
    FilePacking {
        albums: vec![FileAlbum {
            kinds: [
                AttachmentKind::Image,
                AttachmentKind::Gif,
                AttachmentKind::Video,
            ]
            .into(),
            max: ALBUM_MAX,
            ..FileAlbum::default()
        }],
        reply_needs_text: false,
    }
}

/// The longest disappearing-message timer of the fake's Signal-like chats: four weeks, any
/// whole second up to it ([`signal_features`]).
pub const TIMER_MAX_SECS: u32 = 4 * 7 * 24 * 3600;

/// Features of a Signal conversation (§3.8 values from Desktop). Disappearing timers: four
/// values offered by name, and any whole second up to [`TIMER_MAX_SECS`] as a custom time.
pub fn signal_features(group: bool) -> ConversationFeatures {
    let formatting = [
        FormatKind::Bold,
        FormatKind::Italic,
        FormatKind::Strike,
        FormatKind::Spoiler,
        FormatKind::Mono,
        FormatKind::Mention,
    ]
    .into_iter()
    .map(|k| (k, Support::Full))
    .collect();
    let limits = FileLimits {
        max_size: 100 * 1024 * 1024,
        caption: true,
    };
    let files = [
        AttachmentKind::Image,
        AttachmentKind::Video,
        AttachmentKind::Audio,
        AttachmentKind::Voice,
        AttachmentKind::Gif,
        AttachmentKind::File,
    ]
    .into_iter()
    .map(|k| (k, limits))
    .collect();
    ConversationFeatures {
        max_text_bytes: Some(64 * 1024),
        formatting,
        files,
        file_packing: file_packing(),
        reply: Support::Full,
        edit: Some(Window {
            max_age_secs: Some(24 * 3600),
            max_count: Some(10),
        }),
        delete_for_everyone: day_window(1),
        delete_for_me: true,
        edit_receive: day_window(2),
        delete_receive: day_window(2),
        admin_delete: group,
        reactions: Support::Full,
        reactions_per_user: Some(1),
        typing: true,
        read_receipts: true,
        delivery_receipts: true,
        disappearing: BTreeMap::from([(DisappearKind::AfterRead, vec![30, 3600, 86_400, 604_800])]),
        disappearing_custom: BTreeMap::from([(
            DisappearKind::AfterRead,
            TimerRange {
                min_secs: 1,
                max_secs: TIMER_MAX_SECS,
                step_secs: 1,
            },
        )]),
        // (Two of the types Signal Desktop shows but will not save: enough for the composer's
        // hint to be tested on the fake.)
        hard_to_save: ["apk".to_owned(), "exe".to_owned()].into(),
        history: HistorySource::None,
        group_actions: Default::default(),
        cannot_send: None,
    }
}

/// Signal-like (account-wide `(author, timestamp)` ids): catch-up, a receipt before its message,
/// duplicates, an edit of an edit, reaction replacement, a group mention, a delete for everyone,
/// a read marker from the phone, a placeholder replaced by its resend, a PNI→ACI merge with both
/// chats populated, a group system message and typing.
pub fn signal_like() -> Result<Script, ScriptError> {
    let me = user("aci:00000000-0000-4000-8000-0000000000aa")?;
    let me_pni = user("pni:00000000-0000-4000-8000-0000000000ab")?;
    let alice = user("aci:00000000-0000-4000-8000-0000000a11ce")?;
    let bob = user("aci:00000000-0000-4000-8000-000000000b0b")?;
    let carol_pni = user("pni:00000000-0000-4000-8000-0000000ca201")?;
    let carol = user("aci:00000000-0000-4000-8000-0000000ca202")?;
    let group = ConvKey::Group("gv2:ZmFrZS1zaWduYWwtZ3JvdXA".into());
    let d_alice = ConvKey::Direct(alice.clone());
    let d_bob = ConvKey::Direct(bob.clone());
    let d_carol_pni = ConvKey::Direct(carol_pni.clone());
    let d_carol = ConvKey::Direct(carol.clone());
    let one = signal_features(false);
    let many = signal_features(true);
    let ts = |n: i64| T0 + n * 1000;
    let local = |n: i64| ts(n).to_string();

    let settings = AccountSettingsPatch {
        read_receipts: Some(true),
        typing_indicators: Some(true),
        ..Default::default()
    };

    let mut caught_up = remote(&d_alice, &alice, &local(10), ts(10), "Hi there");
    caught_up.catch_up = true;
    let mut mention = remote(&group, &bob, &local(100), ts(100), "\u{FFFC} are you in?");
    mention.body = Body::Text(Text {
        body: "\u{FFFC} are you in?".into(),
        ranges: vec![TextRange {
            start: 0,
            len: 3,
            kind: RangeKind::Mention(me.clone()),
        }],
    });
    mention.mentions_me = true;
    mention.catch_up = true;
    let mut placeholder = remote(&group, &bob, &local(130), ts(130), "");
    placeholder.body = Body::Placeholder(Placeholder::Undecryptable { resendable: true });
    let resent = remote(&group, &bob, &local(130), ts(130), "Saturday works");
    let mut title_changed = remote(&group, &alice, &local(300), ts(300), "");
    title_changed.body = Body::System(SystemEvent::TitleChanged {
        by: Some(alice.clone()),
        title: "Weekend Plans".into(),
    });

    let mut s = Script::new("signal-like", vec![me.clone(), me_pni]);
    s.display_name = Some("Test Account".into());
    let s = s
        .step(Step::Conn(kourier_model::ConnState::Syncing {
            fraction: None,
        }))
        .deliver(vec![
            BackendEvent::AccountSettings(settings),
            contact(&alice, Some("Alice Example"), None),
            contact(&bob, Some("Bob Example"), None),
            BackendEvent::ConversationUpsert(conversation(&d_alice, None, None, &one)),
            BackendEvent::ConversationUpsert(conversation(&d_bob, None, None, &one)),
            BackendEvent::ConversationUpsert(conversation(
                &group,
                Some("Weekend Plans"),
                Some(vec![
                    member(&alice, MemberRole::Admin),
                    member(&bob, MemberRole::Member),
                    member(&me, MemberRole::Member),
                ]),
                &many,
            )),
        ])
        // A delivery receipt for our message arrives before the sync transcript of it.
        .deliver(vec![BackendEvent::Receipts {
            targets: vec![target(None, None, &local(20))],
            by: alice.clone(),
            kind: ReceiptKind::Delivered,
            at: ts(25),
        }])
        .deliver(vec![BackendEvent::Message(caught_up.clone())])
        .deliver(vec![
            BackendEvent::Message(own(&d_alice, &local(20), ts(20), "Hello Alice")),
            BackendEvent::OwnSendState {
                local: None,
                target: target(Some(&d_alice), None, &local(20)),
                per_recipient: vec![(alice.clone(), RecipientState::Sent { sealed: true })],
            },
        ])
        // The same envelope again (a redelivery).
        .deliver(vec![BackendEvent::Message(caught_up)])
        .deliver(vec![BackendEvent::Message(mention)])
        .step(Step::CatchUpDone)
        // An edit, then an edit of the edit (targets the previous revision's timestamp).
        .deliver(vec![BackendEvent::Edited {
            target: target(Some(&d_alice), Some(&alice), &local(10)),
            by: Some(alice.clone()),
            new_local: Some(local(30).into()),
            body: Body::text("Hi there!"),
            edited_at: ts(30),
            keeps_quote: None,
        }])
        .deliver(vec![BackendEvent::Edited {
            target: target(Some(&d_alice), Some(&alice), &local(30)),
            by: Some(alice.clone()),
            new_local: Some(local(40).into()),
            body: Body::text("Hi there!!"),
            edited_at: ts(40),
            keeps_quote: None,
        }])
        .deliver(vec![
            BackendEvent::Reaction {
                target: target(None, None, &local(20)),
                by: Some(alice.clone()),
                emoji: "👍".into(),
                remove: false,
                at: ts(50),
            },
            BackendEvent::Reaction {
                target: target(None, Some(&alice), &local(10)),
                by: None,
                emoji: "❤️".into(),
                remove: false,
                at: ts(51),
            },
        ])
        // Our second reaction replaces the first (one per user).
        .deliver(vec![BackendEvent::Reaction {
            target: target(None, Some(&alice), &local(10)),
            by: None,
            emoji: "👍".into(),
            remove: false,
            at: ts(52),
        }])
        .deliver(vec![BackendEvent::Receipts {
            targets: vec![target(None, None, &local(20))],
            by: alice.clone(),
            kind: ReceiptKind::Read,
            at: ts(60),
        }])
        .deliver(vec![BackendEvent::Message(remote(
            &group,
            &alice,
            &local(110),
            ts(110),
            "Oops, wrong chat",
        ))])
        .deliver(vec![BackendEvent::Deleted {
            target: target(Some(&group), Some(&alice), &local(110)),
            by: Some(alice.clone()),
            for_everyone: true,
            at: ts(120),
        }])
        // We read the group on the phone up to Bob's message.
        .deliver(vec![BackendEvent::ReadMarker {
            conv: None,
            up_to: target(None, Some(&bob), &local(100)),
        }])
        .deliver(vec![BackendEvent::Message(placeholder)])
        .deliver(vec![BackendEvent::Message(resent)])
        .step(Step::Ephemeral(Box::new(BackendEvent::Typing {
            conv: group.clone(),
            by: bob.clone(),
            device: Some(1),
            on: true,
            ttl: Duration::from_secs(15),
        })))
        // Carol writes from her phone number identity, then from her ACI, then the PNI is
        // verified as hers: both direct chats merge.
        .deliver(vec![
            contact(&carol_pni, None, Some("Carol Phone")),
            BackendEvent::Message(remote(
                &d_carol_pni,
                &carol_pni,
                &local(200),
                ts(200),
                "New number, who dis",
            )),
        ])
        .deliver(vec![
            contact(&carol, Some("Carol Example"), None),
            BackendEvent::ConversationUpsert(conversation(&d_carol, None, None, &one)),
            BackendEvent::Message(remote(&d_carol, &carol, &local(210), ts(210), "It's Carol")),
        ])
        .deliver(vec![BackendEvent::IdentityMerged {
            from: carol_pni.clone(),
            into: carol.clone(),
        }])
        .deliver(vec![BackendEvent::Message(remote(
            &d_carol_pni,
            &carol_pni,
            &local(220),
            ts(220),
            "Still me",
        ))])
        .deliver(vec![BackendEvent::Message(title_changed)])
        // The same (author, timestamp) in another conversation is a duplicate (§3.1); which copy
        // wins depends on arrival order.
        .deliver_ordered(vec![BackendEvent::Message(remote(
            &group,
            &alice,
            &local(10),
            ts(10),
            "copy in another conversation",
        ))]);
    Ok(s)
}

fn whatsapp_features() -> ConversationFeatures {
    let mut f = signal_features(false);
    f.edit = Some(Window {
        max_age_secs: Some(15 * 60),
        max_count: None,
    });
    f.edit_receive = Some(Window {
        max_age_secs: Some(20 * 60),
        max_count: None,
    });
    f.history = HistorySource::PushedOnLink;
    f
}

/// The number of messages in the WhatsApp-like group history import.
pub const WHATSAPP_GROUP_HISTORY: usize = 120;

/// The message of the WhatsApp-like group history that was edited before the import (its
/// index), and the id of its newest version.
pub const WHATSAPP_EDITED_HISTORY: (usize, &str) = (7, "3EB0G00007E2");

/// WhatsApp-like (per-conversation ids, pushed history): history imports with own messages and
/// their states, reactions and an unread hint; a history message that was edited before the
/// import, and a live reaction that names its newest version; the same own message in the PN
/// and the LID chat; a PN→LID merge with both chats populated; live traffic after it, an edit
/// without a new id and a delete-for-me sync.
///
/// This network's history says how many messages of a chat are unread, not which: the read
/// state of its rows is UNKNOWN. So every history row says `read: true` and the count is
/// the chat's `unread_hint` (the contract's rule: `read: false` on a history row means the
/// backend knows that the message is unread, and the core counts it).
pub fn whatsapp_like() -> Result<Script, ScriptError> {
    let me = user("lid:900000001")?;
    let me_pn = user("pn:15550000001")?;
    let alice = user("lid:900000101")?;
    let alice_pn = user("pn:15550000101")?;
    let group = ConvKey::Group("wag:120363000000000001".into());
    let d_alice = ConvKey::Direct(alice.clone());
    let d_alice_pn = ConvKey::Direct(alice_pn.clone());
    let f = whatsapp_features();
    let ts = |n: i64| T0 + n * 1000;

    let mut lid_conv = conversation(&d_alice, None, None, &f);
    lid_conv.unread_hint = Some(1);

    let mut h1 = remote(&d_alice_pn, &alice_pn, "3EB0AA01", ts(1), "Old message");
    h1.read = true;
    let mut h2 = own(&d_alice_pn, "3EB0AA02", ts(2), "My old reply");
    h2.initial_out_state = Some(OutgoingState::Read);
    let mut h3 = remote(&d_alice_pn, &alice_pn, "3EB0AA03", ts(3), "Funny one");
    h3.read = true;
    h3.reactions = vec![HistoryReaction {
        by: None,
        emoji: "😂".into(),
        at: ts(4),
    }];
    let mut h4 = remote(&d_alice, &alice, "3EB0BB01", ts(10), "Hello from LID");
    h4.read = true;
    // The same own message, reported in the LID chat with an older state.
    let mut h2_lid = own(&d_alice, "3EB0AA02", ts(2), "My old reply");
    h2_lid.initial_out_state = Some(OutgoingState::Delivered);
    let mut h5 = own(&d_alice, "3EB0BB02", ts(11), "Newer reply");
    h5.initial_out_state = Some(OutgoingState::Delivered);
    let history: Vec<IncomingMessage> = [h1, h2, h3]
        .into_iter()
        .map(|mut m| {
            m.silent = true;
            m
        })
        .collect();
    let history_lid: Vec<IncomingMessage> = [h4, h2_lid, h5]
        .into_iter()
        .map(|mut m| {
            m.silent = true;
            m
        })
        .collect();
    let group_history: Vec<IncomingMessage> = (0..WHATSAPP_GROUP_HISTORY)
        .map(|i| {
            let n = i64::try_from(i).unwrap_or(0);
            let local = format!("3EB0G{i:05}");
            let mut m = if i % 3 == 0 {
                let mut m = own(&group, &local, ts(1000 + n), "group history from me");
                m.initial_out_state = Some(OutgoingState::Read);
                m
            } else {
                remote(&group, &alice, &local, ts(1000 + n), "group history")
            };
            m.silent = true;
            m.read = true;
            if i == WHATSAPP_EDITED_HISTORY.0 {
                // Edited twice before the import: the body is the newest version's, the
                // id and the time are the original's.
                m.body = Body::text("group history, as edited");
                m.edits = vec![
                    HistoryEdit {
                        local: format!("3EB0G{i:05}E1").into(),
                        at: ts(1000 + n) + 300,
                    },
                    HistoryEdit {
                        local: WHATSAPP_EDITED_HISTORY.1.into(),
                        at: ts(1000 + n) + 600,
                    },
                ];
            }
            m
        })
        .collect();

    let mut s = Script::new("whatsapp-like", vec![me.clone(), me_pn]);
    s.login = LoginScript::Auto;
    let s = s
        .deliver(vec![
            BackendEvent::ConversationUpsert(conversation(
                &group,
                Some("Family"),
                Some(vec![
                    member(&alice, MemberRole::Admin),
                    member(&me, MemberRole::Member),
                ]),
                &f,
            )),
            BackendEvent::ConversationUpsert(lid_conv),
            BackendEvent::ConversationUpsert(conversation(&d_alice_pn, None, None, &f)),
            contact(&alice, Some("Alice Example"), None),
        ])
        .deliver(vec![BackendEvent::HistoryBatch {
            conv: d_alice_pn.clone(),
            messages: history,
            has_more: true,
            progress: Some(0.4),
        }])
        .deliver(vec![BackendEvent::HistoryBatch {
            conv: d_alice.clone(),
            messages: history_lid,
            has_more: true,
            progress: Some(0.5),
        }])
        .deliver(vec![BackendEvent::HistoryBatch {
            conv: group.clone(),
            messages: group_history,
            has_more: false,
            progress: Some(1.0),
        }])
        // A reaction to the edited history message that names its newest version: it finds
        // the row through the version's key, also when it comes before the import.
        .deliver(vec![BackendEvent::Reaction {
            target: target(Some(&group), Some(&alice), WHATSAPP_EDITED_HISTORY.1),
            by: None,
            emoji: "👀".into(),
            remove: false,
            at: ts(2000),
        }])
        .deliver(vec![BackendEvent::IdentityMerged {
            from: alice_pn.clone(),
            into: alice.clone(),
        }])
        .deliver(vec![
            BackendEvent::Message(remote(&d_alice, &alice, "3EB0CC01", ts(100), "Live now")),
            BackendEvent::Reaction {
                target: target(Some(&d_alice), None, "3EB0BB02"),
                by: Some(alice.clone()),
                emoji: "👍".into(),
                remove: false,
                at: ts(101),
            },
            BackendEvent::Receipts {
                targets: vec![target(Some(&d_alice), None, "3EB0BB02")],
                by: alice.clone(),
                kind: ReceiptKind::Read,
                at: ts(102),
            },
        ])
        // Still addressed by phone number: resolves to the LID chat.
        .deliver(vec![BackendEvent::Message(remote(
            &d_alice_pn,
            &alice_pn,
            "3EB0CC02",
            ts(110),
            "From the old address",
        ))])
        .deliver(vec![BackendEvent::Edited {
            target: target(Some(&d_alice), Some(&alice), "3EB0CC01"),
            by: Some(alice.clone()),
            new_local: None,
            body: Body::text("Live now (edited)"),
            edited_at: ts(130),
            keeps_quote: None,
        }])
        .deliver(vec![BackendEvent::Deleted {
            target: target(Some(&d_alice_pn), Some(&alice_pn), "3EB0AA03"),
            by: None,
            for_everyone: false,
            at: ts(140),
        }]);
    Ok(s)
}

fn threema_features() -> ConversationFeatures {
    let mut f = signal_features(true);
    f.reactions_per_user = None;
    f.admin_delete = false;
    f.edit_receive = Some(Window {
        max_age_secs: Some(6 * 3600),
        max_count: None,
    });
    f.delete_receive = Some(Window {
        max_age_secs: Some(6 * 3600),
        max_count: None,
    });
    f
}

/// Threema-like (per-conversation u64 ids): references without an author
/// (`TargetAuthor::Unknown`) for reactions, quotes and receipts, a set of reactions per user, a
/// reflected own message, a distribution list with per-member copies, a delete by the author;
/// roaming changes are rejected like a failed D2M transaction.
pub fn threema_like() -> Result<Script, ScriptError> {
    let me = user("threema:OWNID001")?;
    let echo = user("threema:ECHOECHO")?;
    let alice = user("threema:ALICE123")?;
    let group = ConvKey::Group("tgr:ECHOECHO-0000000000000001".into());
    let dlist = ConvKey::Broadcast("tdl:0000000000000001".into());
    let d_echo = ConvKey::Direct(echo.clone());
    let d_alice = ConvKey::Direct(alice.clone());
    let f = threema_features();
    let ts = |n: i64| T0 + n * 1000;

    let mut quote = remote(&group, &alice, "00000000000000a2", ts(2), "Agreed");
    quote.reply_to = Some(ReplyTo {
        target: unknown_target(&group, "00000000000000a1"),
        author: None,
        snapshot: Some(QuoteSnapshot {
            text: Some(Text::plain("Rehearsal at 7?")),
            ..QuoteSnapshot::default()
        }),
    });

    let mut s = Script::new("threema-like", vec![me.clone()]);
    s.reject_changes = true;
    let s = s
        .deliver(vec![
            BackendEvent::ConversationUpsert(conversation(
                &group,
                Some("Choir"),
                Some(vec![
                    member(&echo, MemberRole::Admin),
                    member(&alice, MemberRole::Member),
                    member(&me, MemberRole::Member),
                ]),
                &f,
            )),
            BackendEvent::ConversationUpsert(conversation(&d_echo, None, None, &f)),
            BackendEvent::ConversationUpsert(conversation(&dlist, Some("Announcements"), None, &f)),
            contact(&echo, Some("Echo Test"), None),
        ])
        .deliver(vec![BackendEvent::Message(remote(
            &group,
            &echo,
            "00000000000000a1",
            ts(1),
            "Rehearsal at 7?",
        ))])
        .deliver(vec![
            BackendEvent::Reaction {
                target: unknown_target(&group, "00000000000000a1"),
                by: Some(echo.clone()),
                emoji: "👍".into(),
                remove: false,
                at: ts(5),
            },
            BackendEvent::Reaction {
                target: unknown_target(&group, "00000000000000a1"),
                by: Some(echo.clone()),
                emoji: "❤️".into(),
                remove: false,
                at: ts(6),
            },
        ])
        .deliver(vec![BackendEvent::Reaction {
            target: unknown_target(&group, "00000000000000a1"),
            by: Some(alice.clone()),
            emoji: "👍".into(),
            remove: false,
            at: ts(7),
        }])
        .deliver(vec![BackendEvent::Message(quote)])
        .deliver(vec![BackendEvent::Message(own(
            &d_echo,
            "00000000000000b1",
            ts(20),
            "See you there",
        ))])
        .deliver(vec![BackendEvent::Receipts {
            targets: vec![unknown_target(&d_echo, "00000000000000b1")],
            by: echo.clone(),
            kind: ReceiptKind::Delivered,
            at: ts(21),
        }])
        .deliver(vec![BackendEvent::Receipts {
            targets: vec![unknown_target(&d_echo, "00000000000000b1")],
            by: echo.clone(),
            kind: ReceiptKind::Read,
            at: ts(22),
        }])
        // A distribution-list send and its per-member copies.
        .deliver(vec![
            BackendEvent::Message(own(&dlist, "00000000000000c1", ts(40), "Concert on Friday")),
            BackendEvent::Message(own(
                &d_echo,
                "00000000000000c2",
                ts(40),
                "Concert on Friday",
            )),
            BackendEvent::Message(own(
                &d_alice,
                "00000000000000c3",
                ts(40),
                "Concert on Friday",
            )),
        ])
        .deliver(vec![BackendEvent::Deleted {
            target: unknown_target(&group, "00000000000000a2"),
            by: Some(alice.clone()),
            for_everyone: true,
            at: ts(50),
        }]);
    Ok(s)
}

/// The two waits of [`delete_for_me`], on the account clock.
pub const DELETE_FOR_ME_PAUSE: Duration = Duration::from_secs(30);

/// The chats of [`delete_for_me`]: Dana's (partly cleared), Eli's (deleted) and Fay's (one
/// attachment deleted).
pub fn delete_for_me_chats() -> Result<[ConvKey; 3], ScriptError> {
    Ok([
        ConvKey::Direct(user("aci:00000000-0000-4000-8000-00000000da7a")?),
        ConvKey::Direct(user("aci:00000000-0000-4000-8000-000000000e11")?),
        ConvKey::Direct(user("aci:00000000-0000-4000-8000-000000000fa1")?),
    ])
}

/// The name the fake network gives the attachment number `n` of a message
/// (`Attachment::net_ids`), as [`delete_for_me`] uses it and as the fake states it for the
/// attachments of messages the core sends (`Network::attachment_net_ids`).
pub fn attachment_name(local: &str, n: usize) -> String {
    format!("fake-att:{local}:{n}")
}

/// Signal-like: what a delete for me on the phone can carry besides single messages (Signal
/// `SyncMessage.deleteForMe`). Three direct chats get messages and files; then the phone
///
/// - clears Dana's chat up to our answer (`ConversationCleared`): her photo and the answer
///   go, the message that came after the delete stays;
/// - deletes Eli's chat (`ConversationCleared { remove: true }`): its messages and its file
///   go, the timer notice behind them goes too, and the chat leaves the list; a message Eli
///   sends afterwards brings it back with its title and features;
/// - deletes one of the two files of a message in Fay's chat (`AttachmentDeleted`) and then
///   the only picture of another, which has no caption and goes as a whole.
///
/// The deletes are order-sensitive deliveries (a clear that arrives before its messages finds
/// nothing to clear). The phone deletes [`DELETE_FOR_ME_PAUSE`] after the messages, and Eli
/// writes again as long after that (on the account clock), so a test can look in between.
pub fn delete_for_me() -> Result<Script, ScriptError> {
    use crate::attachments::{synthetic_file, synthetic_image};
    let me = user("aci:00000000-0000-4000-8000-0000000000aa")?;
    let [dana_chat, eli_chat, fay_chat] = delete_for_me_chats()?;
    let peer = |chat: &ConvKey| match chat {
        ConvKey::Direct(u) => u.clone(),
        _ => me.clone(),
    };
    let (dana, eli, fay) = (peer(&dana_chat), peer(&eli_chat), peer(&fay_chat));
    let ts = |n: i64| T0 + n * 1000;
    let named = |mut m: IncomingMessage| {
        if let Body::Media { attachments, .. } = &mut m.body {
            for (n, a) in attachments.iter_mut().enumerate() {
                a.net_ids = vec![attachment_name(&m.local, n)];
            }
        }
        m
    };
    let media = |chat: &ConvKey, from: &UserId, n: i64, files: Vec<kourier_model::Attachment>| {
        let mut m = remote(chat, from, &ts(n).to_string(), ts(n), "");
        m.body = Body::Media {
            attachments: files,
            caption: None,
        };
        named(m)
    };
    let chat = |key: &ConvKey, name: &str| {
        let mut c = ContactPatch::new(peer(key));
        c.profile_name = Patch::Set(name.into());
        let mut p = ConversationPatch::new(key.clone());
        p.features = Some(signal_features(false));
        vec![
            BackendEvent::ContactUpsert(c),
            BackendEvent::ConversationUpsert(p),
        ]
    };
    let mut setup = Vec::new();
    setup.extend(chat(&dana_chat, "Dana Test"));
    setup.extend(chat(&eli_chat, "Eli Test"));
    setup.extend(chat(&fay_chat, "Fay Test"));
    let timer_notice = {
        let mut m = remote(&eli_chat, &eli, &ts(22).to_string(), ts(22), "");
        m.body = Body::System(SystemEvent::TimerChanged {
            by: Some(eli.clone()),
            timer: None,
        });
        m
    };
    let s = Script::new("delete-for-me", vec![me])
        .deliver_ordered(setup)
        .deliver_ordered(vec![
            BackendEvent::Message(media(
                &dana_chat,
                &dana,
                10,
                vec![synthetic_image(320, 240, 10)],
            )),
            BackendEvent::Message(own(&dana_chat, &ts(11).to_string(), ts(11), "Got it")),
            BackendEvent::Message(remote(
                &dana_chat,
                &dana,
                &ts(12).to_string(),
                ts(12),
                "This one came after the delete",
            )),
        ])
        .deliver_ordered(vec![
            BackendEvent::Message(media(
                &eli_chat,
                &eli,
                20,
                vec![synthetic_file("plan.pdf", "application/pdf", 9000, 20)],
            )),
            BackendEvent::Message(own(&eli_chat, &ts(21).to_string(), ts(21), "Thanks")),
            BackendEvent::Message(timer_notice),
        ])
        .deliver_ordered(vec![
            BackendEvent::Message(media(
                &fay_chat,
                &fay,
                30,
                vec![
                    synthetic_file("a.bin", "application/octet-stream", 5000, 30),
                    synthetic_file("b.bin", "application/octet-stream", 6000, 31),
                ],
            )),
            BackendEvent::Message(media(
                &fay_chat,
                &fay,
                31,
                vec![synthetic_image(200, 200, 32)],
            )),
        ])
        .step(Step::WaitAcked)
        .step(Step::Sleep(DELETE_FOR_ME_PAUSE))
        // The phone, afterwards. The anchors are the newest messages it had, newest first;
        // one of them never reached this device.
        .deliver_ordered(vec![BackendEvent::ConversationCleared {
            conv: dana_chat.clone(),
            up_to: vec![
                target(Some(&dana_chat), None, &ts(11).to_string()),
                target(Some(&dana_chat), Some(&dana), &ts(10).to_string()),
                target(Some(&dana_chat), Some(&dana), &ts(5).to_string()),
            ],
            remove: false,
            at: ts(40),
        }])
        .deliver_ordered(vec![BackendEvent::ConversationCleared {
            conv: eli_chat.clone(),
            up_to: vec![target(Some(&eli_chat), None, &ts(21).to_string())],
            remove: true,
            at: ts(41),
        }])
        .deliver_ordered(vec![
            BackendEvent::AttachmentDeleted {
                target: target(Some(&fay_chat), Some(&fay), &ts(30).to_string()),
                net_ids: vec![attachment_name(&ts(30).to_string(), 0)].into(),
                at: ts(42),
            },
            BackendEvent::AttachmentDeleted {
                target: target(Some(&fay_chat), Some(&fay), &ts(31).to_string()),
                net_ids: vec![attachment_name(&ts(31).to_string(), 0)].into(),
                at: ts(43),
            },
        ])
        .step(Step::WaitAcked)
        .step(Step::Sleep(DELETE_FOR_ME_PAUSE))
        .deliver_ordered(vec![BackendEvent::Message(remote(
            &eli_chat,
            &eli,
            &ts(50).to_string(),
            ts(50),
            "Are you still there?",
        ))]);
    Ok(s)
}

/// Every per-network script with its flavor.
pub fn all() -> Result<Vec<(Flavor, Script)>, ScriptError> {
    Ok(vec![
        (Flavor::SignalLike, signal_like()?),
        (Flavor::WhatsAppLike, whatsapp_like()?),
        (Flavor::ThreemaLike, threema_like()?),
    ])
}
