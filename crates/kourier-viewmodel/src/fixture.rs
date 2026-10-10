//! A realistic synthetic data set for `--fixture`, screenshots and tests: three accounts on two
//! networks, thirteen conversations (1:1, groups, note to self, muted, pinned, unread with
//! mentions, a draft) and one long group chat that exercises every row and block kind.
//!
//! Everything is invented. Time is fixed (2026-10-06 14:32 in UTC+3), so labels and
//! screenshots are deterministic on every machine.

use std::collections::HashMap;
use std::time::Duration;

use kourier_model::text::MENTION_PLACEHOLDER;
use kourier_model::{
    Account, AccountId, AccountLocalPrefs, AccountSettings, AccountState, AccountUuid, Attachment,
    AttachmentKind, AttachmentState, Body, ConnState, ConvKey, ConvKind, Conversation,
    ConversationFeatures, ConversationId, Deletion, FailReason, IdError, LoginStep, LoginStepKind,
    Member, MemberRole, Message, MessageId, MsgTarget, NetMsgId, NetworkKind, Origin,
    OutgoingState, QuoteSnapshot, RangeKind, ReadState, Redacted, ReplyTo, SystemEvent, Text,
    TextRange, UserId,
};

use crate::conversations::{
    self, AccountRow, ChatHeader, ConversationInput, ConversationRow, account_row,
};
use crate::names::NameMap;
use crate::source::SourceError;
use crate::time::{CivilDate, FixedOffset, TimeContext, local_instant};
use crate::timeline::{self, MessageRow, Reaction, TimelineInput};

/// The fixture's time zone (Kyiv summer time).
pub const FIXTURE_OFFSET: FixedOffset = FixedOffset(3 * 3600);

/// A message with neutral defaults: net id from the timestamp, sort key = sent time, incoming
/// messages read, own messages read by everyone.
pub fn message(
    id: MessageId,
    conversation: ConversationId,
    origin: Origin,
    sent_at: i64,
    body: Body,
) -> Message {
    let own = origin.is_own();
    let net_id = match origin.author() {
        Some(a) => NetMsgId::remote(a.clone(), sent_at.to_string()),
        None => NetMsgId::own(sent_at.to_string()),
    };
    Message {
        id,
        conversation,
        net_id,
        origin,
        body,
        reply_to: None,
        sent_at,
        server_at: Some(sent_at),
        received_at: sent_at + 900,
        arrival: None,
        sort_key: sent_at,
        revision: 0,
        edited_at: None,
        deleted: None,
        out_state: own.then_some(OutgoingState::Read),
        read_state: (!own).then_some(ReadState::Read),
        expiry: None,
        view_once: false,
        history: false,
        linked: None,
        meta: None,
        raw: None,
    }
}

fn user(scheme: &str, n: u32) -> Result<UserId, IdError> {
    let value = if scheme == "threema" {
        format!("K{n:07}")
    } else {
        format!("{n:08x}-0000-4000-8000-{:012x}", u64::from(n) * 7919)
    };
    UserId::from_parts(scheme, &value)
}

fn day(month: u32, day: u32) -> CivilDate {
    CivilDate {
        year: 2026,
        month,
        day,
    }
}

/// Instant of a local time in the fixture's zone.
pub fn at(date: CivilDate, hour: u32, minute: u32) -> i64 {
    local_instant(date, hour, minute, FIXTURE_OFFSET)
}

fn text(body: &str, ranges: &[(&str, RangeKind)]) -> Text {
    // Ranges are given by the substring they cover (first occurrence).
    let ranges = ranges
        .iter()
        .filter_map(|(needle, kind)| {
            let start = body.find(needle)?;
            Some(TextRange {
                start: start as u32,
                len: needle.len() as u32,
                kind: kind.clone(),
            })
        })
        .collect();
    Text {
        body: body.to_owned(),
        ranges,
    }
}

fn image(w: u32, h: u32, size: u64, blurhash: &str, state: AttachmentState) -> Attachment {
    Attachment {
        kind: AttachmentKind::Image,
        mime: "image/jpeg".into(),
        size,
        dims: Some((w, h)),
        blurhash: Some(blurhash.into()),
        state,
        ..Attachment::default()
    }
}

fn file(name: &str, mime: &str, size: u64, state: AttachmentState) -> Attachment {
    Attachment {
        kind: AttachmentKind::File,
        mime: mime.into(),
        size,
        file_name: Some(name.into()),
        state,
        ..Attachment::default()
    }
}

/// The synthetic data set.
pub struct Fixture {
    /// The fixed "now".
    pub now_ms: i64,
    /// The fixed time zone.
    pub calendar: FixedOffset,
    /// Accounts with their connection state and badge label.
    pub accounts: Vec<(Account, ConnState, &'static str)>,
    /// Contact names (and our own).
    pub names: NameMap,
    /// Conversations.
    pub conversations: Vec<Conversation>,
    /// Messages per conversation, newest first.
    pub messages: HashMap<ConversationId, Vec<Message>>,
    /// Reactions per message.
    pub reactions: HashMap<MessageId, Vec<Reaction>>,
    /// Who is typing where.
    pub typing: HashMap<ConversationId, Vec<UserId>>,
    /// The long group chat.
    pub long_chat: ConversationId,
    next_message: i64,
}

/// Builder state for one conversation's messages (chronological while building).
struct Chat<'f> {
    fx: &'f mut Fixture,
    conv: ConversationId,
    list: Vec<Message>,
}

impl Chat<'_> {
    fn push(&mut self, origin: Origin, sent_at: i64, body: Body) -> &mut Message {
        let id = MessageId(self.fx.next_message);
        self.fx.next_message += 1;
        self.list
            .push(message(id, self.conv, origin, sent_at, body));
        let last = self.list.len() - 1;
        &mut self.list[last]
    }

    fn from(&mut self, author: &UserId, sent_at: i64, body: Body) -> &mut Message {
        self.push(
            Origin::Remote {
                author: author.clone(),
                device: Some(1),
            },
            sent_at,
            body,
        )
    }

    fn own(&mut self, sent_at: i64, body: Body) -> &mut Message {
        self.push(Origin::OwnThisDevice, sent_at, body)
    }

    fn react(&mut self, emoji: &str, who: &[Option<&UserId>]) {
        let Some(m) = self.list.last() else { return };
        let entry = self.fx.reactions.entry(m.id).or_default();
        for (i, u) in who.iter().enumerate() {
            entry.push(Reaction {
                user: u.cloned(),
                emoji: emoji.into(),
                at: m.sent_at + 60_000 * (i as i64 + 1),
            });
        }
    }

    /// Marks the last `n` incoming messages unread.
    fn unread(&mut self, n: usize) {
        for m in self
            .list
            .iter_mut()
            .rev()
            .filter(|m| !m.origin.is_own() && !matches!(m.body, Body::System(_)))
            .take(n)
        {
            m.read_state = Some(ReadState::Unread);
        }
    }

    fn finish(mut self) {
        self.list.sort_by_key(|m| std::cmp::Reverse(m.sort_key));
        let conv = self.conv;
        if let Some(c) = self.fx.conversations.iter_mut().find(|c| c.id == conv)
            && let Some(newest) = self.list.first()
        {
            c.last_activity = newest.sent_at;
        }
        self.fx.messages.insert(conv, self.list);
    }
}

fn reply_to(target: &Message) -> Option<ReplyTo> {
    Some(ReplyTo {
        target: MsgTarget::to_id(None, &target.net_id),
        author: target.origin.author().cloned(),
        snapshot: Some(QuoteSnapshot {
            text: match &target.body {
                Body::Text(t) => Some(t.clone()),
                _ => None,
            },
            attachment: None,
            file_name: None,
            ..QuoteSnapshot::default()
        }),
    })
}

impl Fixture {
    /// Builds the data set. Fails only if the id grammar rejected a fixture id (a bug).
    pub fn build() -> Result<Self, IdError> {
        let today = day(10, 6);
        let mut fx = Fixture {
            now_ms: at(today, 14, 32),
            calendar: FIXTURE_OFFSET,
            accounts: Vec::new(),
            names: NameMap::new(),
            conversations: Vec::new(),
            messages: HashMap::new(),
            reactions: HashMap::new(),
            typing: HashMap::new(),
            long_chat: ConversationId(2),
            next_message: 1,
        };

        // Accounts: personal and work on Signal, one Threema identity.
        let me = user("aci", 1)?;
        let me_work = user("aci", 2)?;
        let me_threema = user("threema", 3)?;
        let account = |id: i64, network, ids: Vec<UserId>, name: &str| Account {
            id: AccountId(id),
            uuid: AccountUuid(uuid::Uuid::from_u128(0x6b6f_7572_6965_7200 + id as u128)),
            network,
            state: AccountState::Active,
            own_ids: ids,
            own_device: Some(2),
            display_name: Some(name.into()),
            settings: AccountSettings::default(),
            local: AccountLocalPrefs::default(),
        };
        fx.accounts = vec![
            (
                account(1, NetworkKind::Signal, vec![me.clone()], "Andrii Melnyk"),
                ConnState::Online { detail: None },
                "",
            ),
            (
                account(2, NetworkKind::Signal, vec![me_work], "Andrii (Work)"),
                ConnState::Syncing {
                    fraction: Some(0.6),
                },
                "Work",
            ),
            (
                account(3, NetworkKind::Threema, vec![me_threema], "Andrii M."),
                ConnState::Offline {
                    retry_at_ms: Some(0),
                },
                "",
            ),
        ];

        // People.
        let people = [
            (10, "Olena Kovalenko"),
            (11, "Taras Hrytsenko"),
            (12, "Dmytro Shevchenko"),
            (13, "Maksym Bondarenko"),
            (14, "Sofia Hnatiuk"),
            (15, "Bohdan Lysenko"),
            (16, "Iryna Bondar"),
            (17, "Mom"),
            (18, "Dad"),
            (19, "Roman Tkachuk"),
            (21, "Kateryna Moroz"),
        ];
        let mut ids: Vec<UserId> = Vec::with_capacity(people.len());
        for (n, name) in people {
            let u = user("aci", n)?;
            fx.names.insert(u.clone(), name);
            ids.push(u);
        }
        let [
            olena,
            taras,
            dmytro,
            maksym,
            sofia,
            bohdan,
            iryna,
            mom,
            dad,
            roman,
            kateryna,
        ] = <[UserId; 11]>::try_from(ids).map_err(|_| IdError::Empty)?;
        let yaroslav = user("threema", 20)?;
        fx.names.insert(yaroslav.clone(), "Yaroslav Kraus");
        fx.names.insert(me.clone(), "Andrii Melnyk");

        // Conversations.
        let signal_features = || {
            let mut f = ConversationFeatures {
                typing: true,
                read_receipts: true,
                delivery_receipts: true,
                reactions: kourier_model::Support::Full,
                reactions_per_user: Some(1),
                reply: kourier_model::Support::Full,
                ..ConversationFeatures::default()
            };
            f.max_text_bytes = Some(64 * 1024);
            f
        };
        let mut conv =
            |id: i64, account: i64, key: ConvKey, title: Option<&str>, members: &[&UserId]| {
                let kind = key.kind();
                fx.conversations.push(Conversation {
                    id: ConversationId(id),
                    account: AccountId(account),
                    key,
                    kind,
                    is_self: false,
                    title: title.map(str::to_owned),
                    avatar: None,
                    members: members
                        .iter()
                        .map(|u| Member {
                            user: (*u).clone(),
                            role: MemberRole::Member,
                        })
                        .collect(),
                    muted_until: None,
                    archived: false,
                    pinned_order: None,
                    blocked: false,
                    request_pending: false,
                    marked_unread: false,
                    timer: None,
                    parent: None,
                    features: signal_features(),
                    local_muted_until: None,
                    draft: None,
                    unread: 0,
                    mentions: 0,
                    read_up_to: None,
                    last_activity: 0,
                    meta: None,
                });
            };
        let group = |s: &str| ConvKey::Group(format!("gv2:{s}").into());
        conv(1, 1, ConvKey::Direct(me.clone()), None, &[&me]);
        conv(
            2,
            1,
            group("ZnB2LXNxdWFk"),
            Some("FPV Squad"),
            &[&me, &olena, &taras, &dmytro, &maksym],
        );
        conv(3, 1, ConvKey::Direct(olena.clone()), None, &[&olena]);
        conv(
            4,
            1,
            group("ZmFtaWx5"),
            Some("Family"),
            &[&me, &mom, &dad, &kateryna],
        );
        conv(5, 1, ConvKey::Direct(dmytro.clone()), None, &[&dmytro]);
        conv(
            6,
            1,
            group("aGlrZQ"),
            Some("Weekend hike"),
            &[&me, &sofia, &roman, &kateryna, &olena, &bohdan],
        );
        conv(7, 2, ConvKey::Direct(iryna.clone()), None, &[&iryna]);
        conv(
            8,
            2,
            group("cmVsZWFzZQ"),
            Some("Release team"),
            &[&me, &iryna, &bohdan, &roman],
        );
        conv(9, 1, ConvKey::Direct(maksym.clone()), None, &[&maksym]);
        conv(10, 3, ConvKey::Direct(yaroslav.clone()), None, &[&yaroslav]);
        conv(11, 1, ConvKey::Direct(sofia.clone()), None, &[&sofia]);
        conv(12, 1, ConvKey::Direct(bohdan.clone()), None, &[&bohdan]);
        conv(13, 1, ConvKey::Direct(roman.clone()), None, &[&roman]);

        let set = |fx: &mut Fixture, id: i64, f: &dyn Fn(&mut Conversation)| {
            if let Some(c) = fx.conversations.iter_mut().find(|c| c.id.0 == id) {
                f(c);
            }
        };
        set(&mut fx, 1, &|c| {
            c.is_self = true;
            c.pinned_order = Some(0);
        });
        set(&mut fx, 2, &|c| {
            c.pinned_order = Some(1);
            c.unread = 3;
            c.mentions = 1;
        });
        set(&mut fx, 3, &|c| c.unread = 2);
        set(&mut fx, 4, &|c| {
            c.muted_until = Some(i64::MAX);
            c.unread = 12;
        });
        set(&mut fx, 7, &|c| c.unread = 1);
        let next_week = at(day(10, 13), 9, 0);
        set(&mut fx, 8, &|c| c.local_muted_until = Some(next_week));
        set(&mut fx, 13, &|c| {
            c.draft = Some(Text::plain("Let me check the motor bell first, then"));
        });
        set(&mut fx, 12, &|c| c.marked_unread = true);

        let (d4, d5, d6) = (day(10, 4), day(10, 5), today);
        let mention = |name_hole: &str, user: &UserId, rest: &str| {
            let body = format!("{name_hole}{MENTION_PLACEHOLDER}{rest}");
            Text {
                ranges: vec![TextRange {
                    start: name_hole.len() as u32,
                    len: MENTION_PLACEHOLDER.len_utf8() as u32,
                    kind: RangeKind::Mention(user.clone()),
                }],
                body,
            }
        };

        // 1: Note to self.
        let mut c = fx.chat(1);
        c.own(
            at(day(10, 2), 21, 14),
            Body::text("Wishlist: TBS Source One v5 frame, 2207 motors"),
        );
        c.own(
            at(d6, 8, 12),
            Body::Text(text(
                "Battery order: 4× 6S 1300 mAh\nCheck https://example.org/lipo first",
                &[("4× 6S 1300 mAh", RangeKind::Bold)],
            )),
        );
        c.finish();

        // 2: FPV Squad, the long chat.
        let mut c = fx.chat(2);
        c.push(
            Origin::Remote {
                author: olena.clone(),
                device: Some(1),
            },
            at(d4, 18, 2),
            Body::System(SystemEvent::MembersAdded {
                by: Some(olena.clone()),
                users: vec![maksym.clone()],
            }),
        );
        c.from(
            &maksym,
            at(d4, 18, 5),
            Body::text("Hey all 👋 thanks for the invite"),
        );
        c.from(
            &olena,
            at(d4, 18, 6),
            Body::Text(text(
                "Welcome! House rules: share your logs, crash fly responsibly",
                &[
                    ("House rules:", RangeKind::Bold),
                    ("share your logs", RangeKind::Italic),
                    ("crash", RangeKind::Strike),
                ],
            )),
        );
        c.react("😂", &[Some(&taras), Some(&maksym)]);
        c.from(
            &taras,
            at(d4, 18, 20),
            Body::Media {
                attachments: vec![image(
                    1600,
                    1200,
                    2_350_000,
                    "LKO2?U%2Tw=w]~RBVZRi};RPxuwH",
                    AttachmentState::Remote,
                )],
                caption: Some(Text::plain("New 5\" build is done 🛠️")),
            },
        );
        c.own(at(d4, 18, 25), Body::text("Looks clean! Which frame?"));
        c.from(
            &taras,
            at(d4, 18, 26),
            Body::text("TBS Source One v5, nothing fancy"),
        );
        c.from(
            &dmytro,
            at(d5, 9, 12),
            Body::text("Betaflight 4.6 release notes: https://betaflight.com/docs/wiki/release/Betaflight-4-6-Release-Notes"),
        );
        c.from(
            &dmytro,
            at(d5, 9, 13),
            Body::Text(text(
                "Note the new dshot_bidir default, check your ESCs before flashing",
                &[("dshot_bidir", RangeKind::Mono)],
            )),
        );
        let m = c.own(
            at(d5, 9, 30),
            Body::text("Already flashed, works fine on BLHeli_32"),
        );
        m.edited_at = Some(at(d5, 9, 32));
        m.revision = 1;
        c.from(
            &olena,
            at(d5, 10, 2),
            Body::text("Who's flying on Saturday?"),
        );
        c.react("👍", &[Some(&taras), Some(&dmytro), None]);
        let m = c.from(&maksym, at(d5, 10, 15), Body::text("(deleted)"));
        m.deleted = Some(Deletion::ForEveryone { by: None });
        m.body = Body::text("");
        c.from(
            &olena,
            at(d5, 20, 10),
            Body::Media {
                attachments: vec![Attachment {
                    kind: AttachmentKind::Voice,
                    mime: "audio/aac".into(),
                    size: 61_000,
                    duration_ms: Some(42_000),
                    state: AttachmentState::Local,
                    ..Attachment::default()
                }],
                caption: None,
            },
        );
        let m = c.own(
            at(d5, 20, 31),
            Body::text("Battery sag looks fine in the log"),
        );
        m.out_state = Some(OutgoingState::Delivered);
        let m = c.own(at(d5, 20, 33), Body::text("🔥"));
        m.out_state = Some(OutgoingState::Delivered);
        let mine = c.own(
            at(d6, 8, 58),
            Body::text("Morning! Weather looks good for Saturday ☀️"),
        );
        let mine = mine.clone();
        let m = c.from(
            &olena,
            at(d6, 9, 5),
            Body::text("Perfect, I'll bring the spare batteries"),
        );
        m.reply_to = reply_to(&mine);
        c.react("❤️", &[None, Some(&taras)]);
        c.from(
            &taras,
            at(d6, 9, 20),
            Body::Media {
                attachments: vec![file(
                    "blackbox_2026-10-06.bbl",
                    "application/octet-stream",
                    4_812_000,
                    AttachmentState::Remote,
                )],
                caption: None,
            },
        );
        c.from(
            &taras,
            at(d6, 9, 21),
            Body::Text(text(
                "Can someone check the gyro noise? I think a prop is bent",
                &[("I think a prop is bent", RangeKind::Spoiler)],
            )),
        );
        let m = c.own(at(d6, 9, 40), Body::text("On it, will look after lunch"));
        m.out_state = Some(OutgoingState::Sent { server_at: None });
        let m = c.own(at(d6, 9, 41), Body::text("Uploading my tune too"));
        m.out_state = Some(OutgoingState::Failed {
            reason: FailReason::Network,
            retryable: true,
        });
        c.push(
            Origin::Remote {
                author: maksym.clone(),
                device: None,
            },
            at(d6, 11, 2),
            Body::System(SystemEvent::IdentityKeyChanged {
                user: maksym.clone(),
            }),
        );
        c.from(
            &dmytro,
            at(d6, 13, 40),
            Body::Text(mention(
                "",
                &me,
                " can you share your rates? Mine are in the doc",
            )),
        );
        c.from(
            &dmytro,
            at(d6, 13, 41),
            Body::text("https://docs.example.org/fpv/rates"),
        );
        c.from(
            &taras,
            at(d6, 13, 55),
            Body::Media {
                attachments: vec![image(
                    1280,
                    853,
                    1_180_000,
                    "L6PZfSi_.AyE_3t7t7R**0o#DgR4",
                    AttachmentState::Remote,
                )],
                caption: Some(Text::plain("Field is mowed 🌾 see you there")),
            },
        );
        c.unread(3);
        c.finish();
        fx.typing.insert(ConversationId(2), vec![olena.clone()]);

        // 3: Olena.
        let mut c = fx.chat(3);
        c.own(at(d5, 19, 0), Body::text("Thanks for the spare props!"));
        c.from(&olena, at(d5, 19, 4), Body::text("Any time 🙂"));
        c.from(
            &olena,
            at(d6, 12, 47),
            Body::text("Did you see the new frame?"),
        );
        c.from(
            &olena,
            at(d6, 12, 48),
            Body::text("Are we still on for Saturday?"),
        );
        c.unread(2);
        c.finish();

        // 4: Family (muted).
        let mut c = fx.chat(4);
        c.from(
            &dad,
            at(d6, 10, 3),
            Body::text("Who's coming for dinner on Sunday?"),
        );
        c.from(
            &mom,
            at(d6, 11, 47),
            Body::Media {
                attachments: vec![image(
                    1200,
                    1600,
                    1_900_000,
                    "LEHV6nWB2yk8pyo0adR*.7kCMdnj",
                    AttachmentState::Remote,
                )],
                caption: None,
            },
        );
        c.unread(2);
        c.finish();

        // 5: Dmytro: last message is ours and still pending.
        let mut c = fx.chat(5);
        c.from(
            &dmytro,
            at(d6, 10, 30),
            Body::text("Can you send me your config?"),
        );
        let m = c.own(at(d6, 11, 20), Body::text("Sent you the config 👍"));
        m.out_state = Some(OutgoingState::Sending { progress: None });
        c.finish();

        // 6: Weekend hike.
        let mut c = fx.chat(6);
        c.from(&roman, at(d5, 17, 2), Body::text("Route is on Komoot"));
        c.from(
            &sofia,
            at(d5, 18, 15),
            Body::text("Who's bringing the stove?"),
        );
        c.finish();

        // 7: Iryna (work account).
        let mut c = fx.chat(7);
        c.own(
            at(d6, 11, 0),
            Body::text("Pushed the fix for the login flow"),
        );
        c.from(
            &iryna,
            at(d6, 13, 58),
            Body::text("Can you review the PR today?"),
        );
        c.unread(1);
        c.finish();

        // 8: Release team (work, muted for a week).
        let mut c = fx.chat(8);
        c.from(
            &bohdan,
            at(day(10, 5), 16, 30),
            Body::text("v0.3 is tagged, release notes in the wiki"),
        );
        c.finish();

        // 9: Maksym: a photo.
        let mut c = fx.chat(9);
        c.from(
            &maksym,
            at(day(10, 3), 16, 40),
            Body::Media {
                attachments: vec![image(
                    1600,
                    900,
                    2_100_000,
                    "LGF5]+Yk^6#M@-5c,1J5@[or[Q6.",
                    AttachmentState::Remote,
                )],
                caption: None,
            },
        );
        c.finish();

        // 10: Yaroslav on Threema.
        let mut c = fx.chat(10);
        c.own(
            at(day(10, 2), 18, 0),
            Body::text("Booked the tickets for the meetup"),
        );
        c.from(
            &yaroslav,
            at(day(10, 2), 18, 30),
            Body::text("Danke! See you in Berlin"),
        );
        c.finish();

        // 11: Sofia: deleted last message.
        let mut c = fx.chat(11);
        c.own(at(day(9, 28), 20, 0), Body::text("Happy birthday! 🎉"));
        let m = c.from(&sofia, at(day(9, 28), 20, 12), Body::text(""));
        m.deleted = Some(Deletion::ForEveryone { by: None });
        c.finish();

        // 12: Bohdan: a file, marked unread.
        let mut c = fx.chat(12);
        c.from(
            &bohdan,
            at(day(9, 20), 15, 0),
            Body::Media {
                attachments: vec![file(
                    "flight-log-0412.pdf",
                    "application/pdf",
                    830_000,
                    AttachmentState::Local,
                )],
                caption: None,
            },
        );
        c.finish();

        // 13: Roman: an old chat with a draft.
        let mut c = fx.chat(13);
        let last_year = CivilDate {
            year: 2025,
            month: 12,
            day: 30,
        };
        c.from(
            &roman,
            at(last_year, 22, 15),
            Body::text("Motor 3 sounds rough after the crash"),
        );
        c.finish();

        // Timer change as a system event, to show a system row with an icon in a 1:1 chat.
        if let Some(list) = fx.messages.get_mut(&ConversationId(3)) {
            let id = MessageId(fx.next_message);
            fx.next_message += 1;
            let mut m = message(
                id,
                ConversationId(3),
                Origin::OwnThisDevice,
                at(d5, 18, 0),
                Body::System(SystemEvent::TimerChanged {
                    by: None,
                    timer: Some(Duration::from_secs(7 * 86_400)),
                }),
            );
            m.out_state = None;
            list.push(m);
            list.sort_by_key(|m| std::cmp::Reverse(m.sort_key));
        }
        Ok(fx)
    }

    fn chat(&mut self, id: i64) -> Chat<'_> {
        Chat {
            fx: self,
            conv: ConversationId(id),
            list: Vec::new(),
        }
    }

    fn account_of(&self, id: AccountId) -> Option<&(Account, ConnState, &'static str)> {
        self.accounts.iter().find(|(a, ..)| a.id == id)
    }

    /// Drawer rows.
    pub fn account_rows(&self) -> Vec<AccountRow> {
        self.accounts
            .iter()
            .map(|(a, conn, _)| {
                let unread = self
                    .conversations
                    .iter()
                    .filter(|c| c.account == a.id && !conversations::is_muted(c, self.now_ms))
                    .map(|c| c.unread)
                    .sum();
                account_row(a, conn, unread)
            })
            .collect()
    }

    /// Conversation-list rows (unsorted).
    pub fn conversation_rows(&self, time: &TimeContext<'_>) -> Vec<ConversationRow> {
        let empty = Vec::new();
        self.conversations
            .iter()
            .map(|c| {
                let (network, label) = self
                    .account_of(c.account)
                    .map_or((NetworkKind::Signal, ""), |(a, _, l)| (a.network, *l));
                conversations::conversation_row(
                    &ConversationInput {
                        conversation: c,
                        last: self.messages.get(&c.id).and_then(|m| m.first()),
                        typing: self.typing.get(&c.id).unwrap_or(&empty),
                        network,
                        multi_account: self.accounts.len() > 1,
                        account_label: label,
                    },
                    &self.names,
                    time,
                )
            })
            .collect()
    }

    /// The chat header of `id`.
    pub fn chat_header(&self, id: ConversationId, time: &TimeContext<'_>) -> Option<ChatHeader> {
        let c = self.conversations.iter().find(|c| c.id == id)?;
        let network = self
            .account_of(c.account)
            .map_or(NetworkKind::Signal, |(a, ..)| a.network);
        let typing = self.typing.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        Some(conversations::chat_header(
            c,
            typing,
            network,
            &self.names,
            time,
        ))
    }

    /// Timeline rows of `id`, newest first.
    pub fn timeline_rows(&self, id: ConversationId, time: &TimeContext<'_>) -> Vec<MessageRow> {
        let Some(messages) = self.messages.get(&id) else {
            return Vec::new();
        };
        let is_group = self
            .conversations
            .iter()
            .find(|c| c.id == id)
            .is_some_and(|c| c.kind != ConvKind::Direct);
        timeline::build_timeline(
            &TimelineInput {
                messages,
                reactions: &self.reactions,
                is_group,
                unread_from: timeline::first_unread(messages),
                has_older: false,
            },
            &self.names,
            time,
        )
    }

    /// The QR step of the fixture login (an invented, unusable link).
    pub fn login_qr(&self) -> LoginStep {
        LoginStep {
            step_id: "qr".into(),
            txn: 1,
            kind: LoginStepKind::ShowQr {
                data: Redacted::new(
                    "sgnl://linkdevice?uuid=Zml4dHVyZS1ub3QtYS1yZWFsLWxpbms&pub_key=BWtvdXJpZXItZml4dHVyZS1rZXktbm90LXJlYWwtMDAwMDAwMDAwMA"
                        .to_owned(),
                ),
                valid_for: Some(Duration::from_secs(60)),
            },
        }
    }

    /// Appends an own, queued text message (the fixture's "send").
    pub fn send_text(
        &mut self,
        id: ConversationId,
        text: Text,
        reply: Option<MessageId>,
    ) -> Result<MessageId, SourceError> {
        let conv = self
            .conversations
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or(SourceError::UnknownConversation)?;
        if let Some(reason) = &conv.features.cannot_send {
            return Err(SourceError::CannotSend(timeline::cannot_send_text(reason)));
        }
        let list = self.messages.entry(id).or_default();
        let sent_at = list
            .first()
            .map_or(self.now_ms, |m| m.sent_at.max(self.now_ms - 1) + 1);
        conv.last_activity = sent_at;
        conv.draft = None;
        let mid = MessageId(self.next_message);
        self.next_message += 1;
        let mut m = message(mid, id, Origin::OwnThisDevice, sent_at, Body::Text(text));
        m.out_state = Some(OutgoingState::Queued);
        m.reply_to = reply
            .and_then(|r| list.iter().find(|m| m.id == r))
            .and_then(reply_to);
        list.insert(0, m);
        Ok(mid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::ConversationFilter;
    use crate::timeline::{Block, Receipt, RowKind};

    #[test]
    fn data_set_shape() {
        let fx = Fixture::build().expect("fixture");
        assert_eq!(fx.accounts.len(), 3);
        assert!(fx.conversations.len() >= 12);
        let time = TimeContext::new(fx.now_ms, &fx.calendar);
        let rows = ConversationFilter::default().apply(fx.conversation_rows(&time));
        assert_eq!(rows[0].title, "Note to Self");
        assert_eq!(rows[1].title, "FPV Squad");
        assert!(rows[1].typing.contains("Olena"));
        assert!(rows.iter().any(|r| r.muted && r.unread > 0));
        assert!(rows.iter().any(|r| r.mentions > 0));
        assert!(rows.iter().any(
            |r| r.preview_author == "Draft" && r.preview_source == crate::PreviewSource::Draft
        ));
        // Group previews name the peer; own previews carry the receipt.
        let squad = &rows[1];
        assert_eq!(squad.preview_source, crate::PreviewSource::Peer);
        assert!(
            rows.iter()
                .filter(|r| r.receipt != Receipt::None)
                .all(|r| r.preview_source == crate::PreviewSource::Own)
        );
        assert!(rows.iter().any(|r| r.receipt == Receipt::Pending));
        assert!(rows.iter().any(|r| r.account_badge == "Work"));
        assert!(rows.iter().all(|r| !r.badge_icon.is_empty()));
        assert!(rows.iter().any(|r| r.badge_icon == "threema"));
        let times: Vec<&str> = rows.iter().map(|r| r.time_label.as_str()).collect();
        assert!(
            times.contains(&"Yesterday") && times.contains(&"30 Dec 2025"),
            "{times:?}"
        );
        let accounts = fx.account_rows();
        assert_eq!(accounts[0].status.as_str(), "online");
        assert_eq!(accounts[1].status.as_str(), "connecting");
    }

    #[test]
    fn long_chat_covers_every_kind() {
        let fx = Fixture::build().expect("fixture");
        let time = TimeContext::new(fx.now_ms, &fx.calendar);
        let rows = fx.timeline_rows(fx.long_chat, &time);
        let kinds: std::collections::BTreeSet<&str> =
            rows.iter().map(|r| r.kind.as_str()).collect();
        for k in ["message", "date", "unread", "system"] {
            assert!(kinds.contains(k), "missing {k}");
        }
        let receipts: std::collections::BTreeSet<&str> =
            rows.iter().map(|r| r.receipt.as_str()).collect();
        for r in ["sent", "delivered", "read", "failed"] {
            assert!(receipts.contains(r), "missing receipt {r}");
        }
        assert!(rows.iter().any(|r| r.edited));
        assert!(rows.iter().any(|r| r.deleted));
        assert!(rows.iter().any(|r| !r.reactions.is_empty()));
        assert!(rows.iter().any(|r| r.show_author && r.show_avatar));
        let texts: Vec<&crate::richtext::RichText> = rows
            .iter()
            .flat_map(|r| &r.blocks)
            .filter_map(|b| match b {
                Block::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| !t.spoilers.is_empty()));
        assert!(texts.iter().any(|t| t.jumbo));
        assert!(texts.iter().any(|t| t.html.contains("<code>")));
        assert!(texts.iter().any(|t| t.html.contains("<a href=\"https://")));
        assert!(texts.iter().any(|t| t.plain.starts_with("@Andrii")));
        let unread = rows
            .iter()
            .find(|r| r.kind == RowKind::UnreadMarker)
            .expect("marker");
        assert_eq!(unread.text, "3 unread messages");
        let header = fx.chat_header(fx.long_chat, &time).expect("header");
        assert_eq!(header.subtitle, "Olena is typing…");
        assert!(header.typing && header.is_group);
    }

    #[test]
    fn debug_output_is_log_safe() {
        // §9.1: a `{:?}` of a row must not leak names or message text.
        let fx = Fixture::build().expect("fixture");
        let time = TimeContext::new(fx.now_ms, &fx.calendar);
        let mut dumps = vec![
            format!("{:?}", fx.names),
            format!("{:?}", fx.account_rows()),
        ];
        dumps.push(format!("{:?}", fx.conversation_rows(&time)));
        for c in &fx.conversations {
            dumps.push(format!("{:?}", fx.timeline_rows(c.id, &time)));
            dumps.push(format!("{:?}", fx.chat_header(c.id, &time)));
        }
        let filter = ConversationFilter {
            account: None,
            query: "Olena".into(),
        };
        dumps.push(format!("{filter:?}"));
        for secret in [
            "Olena",
            "Andrii",
            "Taras",
            "FPV Squad",
            "gyro",
            "batteries",
            "blackbox",
        ] {
            for d in &dumps {
                assert!(!d.contains(secret), "{secret} in a Debug dump");
            }
        }
    }
}
