//! Deterministic synthetic data for UI fixture mode (`kourier --fixture`, §10.2) and screenshot
//! tests: contacts, direct chats and groups with formatted text, replies, reactions, receipts,
//! pinned, archived and muted conversations. Every name is generated from syllable tables; no
//! real people.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::{
    AttachmentKind, Body, ConnState, ContactPatch, ConvKey, ConversationPatch, DisappearKind,
    Expiry, Member, MemberList, MemberRole, MsgTarget, NetMsgId, Patch, QuoteSnapshot, RangeKind,
    ReceiptKind, RecipientState, ReplyTo, SystemEvent, Text, TextRange, UserAction, UserId,
};

use crate::script::{Script, ScriptError, Step};
use crate::scripts::{own, remote, signal_features};

/// What to generate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureSpec {
    /// PRNG seed: the same seed gives the same data.
    pub seed: u64,
    /// Number of contacts (each gets a direct chat).
    pub contacts: usize,
    /// Number of groups.
    pub groups: usize,
    /// Messages per conversation.
    pub messages_per_conversation: usize,
    /// Timestamp of the first message (ms).
    pub start_ms: i64,
    /// A few conversations get a synthetic photo (downloaded through the core's attachment
    /// pipeline, [`crate::attachments`]).
    pub images: bool,
    /// A few direct chats and every group get an edited message, a message deleted for
    /// everyone and a reaction on one of ours near their newest messages (the edited marker, a
    /// tombstone and reactions to show).
    pub modifiers: bool,
    /// Two more direct chats, newer than everything else, for what the rest does not show
    /// ([`showcase_quotes`], [`showcase_timer`]): quotes of a known, a missing and a deleted
    /// message, a quoted photo, a photo whose caption was edited, and a chat with a
    /// disappearing-message timer, its timer notices and messages that carry the timer. Off by
    /// default: the default fixture stays what it was.
    pub showcase: bool,
    /// The state the account ends in once everything is delivered.
    pub state: FixtureState,
}

/// Where the fixture account ends up, for the account-state banners.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FixtureState {
    /// Connected and caught up.
    #[default]
    Online,
    /// `ConnState::Offline` without a retry time.
    Offline,
    /// `ConnState::Connecting`.
    Connecting,
    /// `ConnState::Syncing` at 40 %.
    Syncing,
    /// `ConnState::NeedsUserAction(SolveChallenge)` with [`CHALLENGE_URL`]; any answer
    /// (`UiCommand::SubmitChallenge`) is accepted and the account goes `Online`.
    Challenge,
    /// `ConnState::NeedsUserAction(FixClock)`: the clock is two hours ahead.
    ClockSkew,
    /// The device is unlinked (`AccountState::LoggedOut`, history kept; "Link again" works).
    LoggedOut,
}

/// Where the fixture's challenge is "solved". Never opened by a test; `.invalid` does not
/// resolve.
pub const CHALLENGE_URL: &str = "https://challenge.invalid/kourier-fixture";

/// The peer of the showcase chat with quotes and an edited photo caption
/// ([`FixtureSpec::showcase`]): `ConvKey::Direct` of this user.
pub fn showcase_quotes() -> Result<ConvKey, ScriptError> {
    Ok(ConvKey::Direct(fixture_user(0x1001)?))
}

/// The peer of the showcase chat with a disappearing-message timer
/// ([`FixtureSpec::showcase`]): `ConvKey::Direct` of this user.
pub fn showcase_timer() -> Result<ConvKey, ScriptError> {
    Ok(ConvKey::Direct(fixture_user(0x1002)?))
}

impl Default for FixtureSpec {
    fn default() -> Self {
        Self {
            seed: 1,
            contacts: 12,
            groups: 4,
            messages_per_conversation: 40,
            start_ms: crate::scripts::T0,
            images: true,
            modifiers: true,
            showcase: false,
            state: FixtureState::Online,
        }
    }
}

/// Texts of the fixture's edits (one per conversation, by index).
const EDITS: &[&str] = &[
    "Sorry, I meant tomorrow at ten.",
    "Make that two tickets, not three.",
    "Fixed the address: it is number 12.",
    "Actually the park, not the station.",
    "Corrected: dinner starts at eight.",
];

/// Modifiers on the newest text messages of one conversation (`sent`, oldest first): the newest
/// incoming message is edited by its author, the second-newest own message is deleted for
/// everyone (from our other device), and a member reacts to the newest own message. All inside
/// the receive windows (§3.8).
fn modifiers(
    key: &ConvKey,
    members: &[UserId],
    sent: &[NetMsgId],
    now: i64,
    edit: &str,
) -> Vec<BackendEvent> {
    let sent_at = |id: &NetMsgId| id.local.parse::<i64>().unwrap_or(now);
    let mut events = Vec::new();
    if let Some(id) = sent.iter().rev().find(|id| id.author.is_some()) {
        let at = sent_at(id) + 90_000;
        events.push(BackendEvent::Edited {
            target: MsgTarget::to_id(Some(key.clone()), id),
            by: id.author.clone(),
            new_local: Some(at.to_string().into()),
            body: Body::Text(Text::plain(edit)),
            edited_at: at,
            keeps_quote: None,
        });
    }
    let own: Vec<&NetMsgId> = sent.iter().rev().filter(|id| id.author.is_none()).collect();
    if let Some(id) = own.get(1) {
        events.push(BackendEvent::Deleted {
            target: MsgTarget::to_id(Some(key.clone()), id),
            by: None,
            for_everyone: true,
            at: sent_at(id) + 60_000,
        });
    }
    if let (Some(id), Some(by)) = (own.first(), members.first()) {
        events.push(BackendEvent::Reaction {
            target: MsgTarget::to_id(Some(key.clone()), id),
            by: Some(by.clone()),
            emoji: "❤️".into(),
            remove: false,
            at: sent_at(id) + 45_000,
        });
    }
    events
}

/// SplitMix64: small, fast, deterministic.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`0` when `n == 0`).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % u64::try_from(n).unwrap_or(u64::MAX)).unwrap_or(0)
    }

    /// `true` with probability `percent` / 100.
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// One element of `items`.
    pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items.get(self.below(items.len())).copied().unwrap_or("")
    }
}

const FIRST: &[&str] = &[
    "Ari", "Bex", "Cato", "Dima", "Eno", "Fenna", "Gali", "Hux", "Ivo", "Juno", "Kel", "Lio",
    "Mira", "Noor", "Oda", "Pim", "Quin", "Rue", "Sol", "Tamsin", "Uli", "Vesna", "Wren", "Yara",
];
const LAST: &[&str] = &[
    "Arden", "Brisk", "Coral", "Dune", "Ember", "Fable", "Grove", "Harbor", "Isle", "Jasper",
    "Kestrel", "Lumen", "Marrow", "Nimbus", "Onyx", "Pebble", "Quill", "Ridge", "Sable", "Thistle",
];
const GROUPS: &[&str] = &[
    "Book Club",
    "Hiking Crew",
    "Band Practice",
    "Garden Swap",
    "Board Games",
    "Climbing Gym",
    "Recipe Exchange",
    "Night Owls",
    "Bike Repair",
    "Choir",
];
const WORDS: &[&str] = &[
    "the",
    "a",
    "we",
    "could",
    "meet",
    "later",
    "tomorrow",
    "coffee",
    "train",
    "park",
    "music",
    "weekend",
    "plan",
    "maybe",
    "sounds",
    "good",
    "great",
    "idea",
    "photos",
    "from",
    "yesterday",
    "running",
    "late",
    "sorry",
    "dinner",
    "at",
    "seven",
    "bring",
    "snacks",
    "map",
    "trail",
    "weather",
    "looks",
    "fine",
    "rain",
    "again",
    "ticket",
    "booked",
    "thanks",
    "see",
    "you",
    "there",
    "tonight",
    "office",
    "garden",
    "tomatoes",
    "recipe",
    "soup",
    "practice",
    "chords",
];

fn sentence(rng: &mut Rng) -> String {
    let n = 3 + rng.below(14);
    let mut words: Vec<&str> = (0..n).map(|_| rng.pick(WORDS)).collect();
    let mut s = String::new();
    if let Some(first) = words.first_mut() {
        let mut c = first.chars();
        if let Some(f) = c.next() {
            s.extend(f.to_uppercase());
            s.push_str(c.as_str());
        }
    }
    for w in words.iter().skip(1) {
        s.push(' ');
        s.push_str(w);
    }
    s.push(*['.', '!', '?', '.'].get(rng.below(4)).unwrap_or(&'.'));
    s
}

fn formatted(rng: &mut Rng) -> Body {
    let body = sentence(rng);
    let mut ranges = Vec::new();
    if rng.chance(15)
        && let Some(space) = body.find(' ')
    {
        ranges.push(TextRange {
            start: 0,
            len: u32::try_from(space).unwrap_or(0),
            kind: if rng.chance(50) {
                RangeKind::Bold
            } else {
                RangeKind::Italic
            },
        });
    }
    Body::Text(Text { body, ranges })
}

fn fixture_user(n: u64) -> Result<UserId, ScriptError> {
    Ok(UserId::parse(&format!(
        "aci:00000000-0000-4000-8000-{:012x}",
        0x00f1_0000 + n
    ))?)
}

/// Builds the fixture script: an automatic login, then contacts, conversations and messages in
/// deliveries of at most 50 events.
pub fn fixture(spec: &FixtureSpec) -> Result<Script, ScriptError> {
    let mut rng = Rng::new(spec.seed);
    let me = fixture_user(0)?;
    let mut s = Script::new("fixture", vec![me.clone()]);
    s.display_name = Some("Demo User".into());

    let mut people = Vec::new();
    let mut setup = Vec::new();
    for i in 0..spec.contacts {
        let user = fixture_user(u64::try_from(i).unwrap_or(0) + 1)?;
        let name = format!("{} {}", rng.pick(FIRST), rng.pick(LAST));
        let mut p = ContactPatch::new(user.clone());
        p.profile_name = Patch::Set(name);
        setup.push(BackendEvent::ContactUpsert(p));
        people.push(user);
    }
    let mut convs: Vec<(ConvKey, Vec<UserId>)> = people
        .iter()
        .map(|u| (ConvKey::Direct(u.clone()), vec![u.clone()]))
        .collect();
    for g in 0..spec.groups {
        let key = ConvKey::Group(format!("gv2:Zml4dHVyZS1ncm91cC0{g:03}").into());
        let size = (3 + rng.below(4)).min(people.len());
        let mut members: Vec<UserId> = Vec::new();
        while members.len() < size {
            let Some(u) = people.get(rng.below(people.len())) else {
                break;
            };
            if !members.contains(u) {
                members.push(u.clone());
            }
        }
        let mut patch = ConversationPatch::new(key.clone());
        patch.title = Patch::Set(
            GROUPS
                .get(g % GROUPS.len())
                .copied()
                .unwrap_or("Group")
                .into(),
        );
        let mut list: Vec<Member> = members
            .iter()
            .enumerate()
            .map(|(i, u)| Member {
                user: u.clone(),
                role: if i == 0 {
                    MemberRole::Admin
                } else {
                    MemberRole::Member
                },
            })
            .collect();
        list.push(Member {
            user: me.clone(),
            role: MemberRole::Member,
        });
        patch.members = Some(MemberList::snapshot(list));
        patch.features = Some(signal_features(true));
        setup.push(BackendEvent::ConversationUpsert(patch));
        convs.push((key, members));
    }
    for (i, (key, _)) in convs.iter().enumerate() {
        let mut patch = ConversationPatch::new(key.clone());
        if key.direct_peer().is_some() {
            patch.features = Some(signal_features(false));
        }
        match i {
            0 => patch.pinned_order = Patch::Set(0),
            1 => patch.pinned_order = Patch::Set(1),
            2 => patch.muted_until = Patch::Set(i64::MAX),
            3 => patch.archived = Some(true),
            _ => {}
        }
        setup.push(BackendEvent::ConversationUpsert(patch));
    }
    for chunk in setup.chunks(50) {
        s = s.deliver(chunk.to_vec());
    }

    let mut t = spec.start_ms;
    let mut events = Vec::new();
    for (ci, (key, members)) in convs.iter().enumerate() {
        let mut sent: Vec<NetMsgId> = Vec::new();
        let mut photos: Vec<NetMsgId> = Vec::new();
        for m in 0..spec.messages_per_conversation {
            t += i64::try_from(30_000 + rng.below(90 * 60_000)).unwrap_or(0);
            let local = t.to_string();
            let author = members.get(rng.below(members.len()));
            let mine = author.is_none() || rng.chance(40);
            let mut msg = match author {
                Some(a) if !mine => remote(key, a, &local, t, ""),
                _ => own(key, &local, t, ""),
            };
            msg.body = formatted(&mut rng);
            // A photo a few rows above the newest message of some chats (the random stream
            // stays the same, so the rest of the fixture does not move).
            let photo_at = spec.messages_per_conversation.checked_sub(4);
            if spec.images && Some(m) == photo_at {
                let (w, h) = match ci {
                    0 => (640, 480),
                    1 => (480, 640),
                    4 => (800, 450),
                    _ => (0, 0),
                };
                if w > 0 {
                    let caption = matches!(ci, 0 | 4).then(|| Text {
                        body: "Photos from yesterday".into(),
                        ranges: Vec::new(),
                    });
                    let seed = spec.seed ^ u64::try_from(ci).unwrap_or(0);
                    msg.body = Body::Media {
                        attachments: vec![crate::attachments::synthetic_image(w, h, seed)],
                        caption,
                    };
                }
            }
            // The newest messages of a few conversations stay unread.
            let tail = spec.messages_per_conversation.saturating_sub(m);
            msg.read = !(ci % 3 == 0 && tail <= 3);
            if let (Some(prev), true) = (sent.last(), rng.chance(10)) {
                msg.reply_to = Some(ReplyTo {
                    target: MsgTarget::to_id(Some(key.clone()), prev),
                    author: prev.author.clone(),
                    snapshot: None,
                });
            }
            let id = msg.net_id();
            if matches!(msg.body, Body::Media { .. }) {
                photos.push(id.clone());
            }
            events.push(BackendEvent::Message(msg));
            if mine {
                let per = members
                    .iter()
                    .map(|u| (u.clone(), RecipientState::Sent { sealed: true }))
                    .collect();
                events.push(BackendEvent::OwnSendState {
                    local: None,
                    target: MsgTarget::to_id(Some(key.clone()), &id),
                    per_recipient: per,
                });
                for u in members {
                    let kind = if rng.chance(70) {
                        ReceiptKind::Read
                    } else {
                        ReceiptKind::Delivered
                    };
                    events.push(BackendEvent::Receipts {
                        targets: vec![MsgTarget::to_id(Some(key.clone()), &id)],
                        by: u.clone(),
                        kind,
                        at: t + 60_000,
                    });
                }
            } else if rng.chance(15) {
                events.push(BackendEvent::Reaction {
                    target: MsgTarget::to_id(Some(key.clone()), &id),
                    by: None,
                    emoji: rng.pick(&["👍", "❤️", "😂", "🎉"]).into(),
                    remove: false,
                    at: t + 30_000,
                });
            }
            sent.push(id);
        }
        // The edited marker, a tombstone and a reaction on one of ours, without drawing from
        // the random stream (the rest of the fixture stays the same).
        if spec.modifiers && (matches!(ci, 0 | 1 | 4) || key.direct_peer().is_none()) {
            let texts: Vec<NetMsgId> = sent
                .iter()
                .filter(|id| !photos.contains(id))
                .cloned()
                .collect();
            let edit = EDITS.get(ci % EDITS.len()).copied().unwrap_or("Edited.");
            events.extend(modifiers(key, members, &texts, t, edit));
        }
        // Read on another device: everything older than the unread tail.
        if ci % 3 != 0
            && let Some(last) = sent.iter().rev().find(|id| id.author.is_some())
        {
            events.push(BackendEvent::ReadMarker {
                conv: Some(key.clone()),
                up_to: MsgTarget::to_id(Some(key.clone()), last),
            });
        }
    }
    if spec.showcase {
        events.extend(showcase(t + 3_600_000)?);
    }
    for chunk in events.chunks(50) {
        s = s.deliver(chunk.to_vec());
    }
    match spec.state {
        FixtureState::Online => {}
        FixtureState::Offline => {
            s = s.step(Step::Conn(ConnState::Offline { retry_at_ms: None }));
        }
        FixtureState::Connecting => s = s.step(Step::Conn(ConnState::Connecting)),
        FixtureState::Syncing => {
            s = s.step(Step::Conn(ConnState::Syncing {
                fraction: Some(0.4),
            }));
        }
        FixtureState::Challenge => {
            s = s.step(Step::Conn(ConnState::NeedsUserAction(
                UserAction::SolveChallenge {
                    url: CHALLENGE_URL.into(),
                },
            )));
        }
        FixtureState::ClockSkew => {
            s = s.step(Step::Conn(ConnState::NeedsUserAction(
                UserAction::FixClock {
                    skew_ms: 2 * 3_600_000,
                },
            )));
        }
        FixtureState::LoggedOut => {
            s = s.step(Step::Unlink {
                reason: "this device was unlinked".into(),
            });
        }
    }
    Ok(s)
}

/// The timer of the showcase chat's messages: one week (a timer `signal_features` lists).
const SHOWCASE_TIMER: Duration = Duration::from_secs(604_800);

/// The showcase chats ([`FixtureSpec::showcase`]), starting at `t0` (ms). Nothing here draws
/// from the fixture's random stream.
fn showcase(t0: i64) -> Result<Vec<BackendEvent>, ScriptError> {
    let mut events = Vec::new();
    let at = |minutes: i64| t0 + minutes * 60_000;
    let local = |minutes: i64| at(minutes).to_string();
    let text_of = |text: &str| QuoteSnapshot {
        text: Some(Text::plain(text)),
        ..QuoteSnapshot::default()
    };

    // ---- quotes, and a photo whose caption was edited ----
    let key = showcase_quotes()?;
    let peer = fixture_user(0x1001)?;
    events.extend(chat(&key, &peer, "Pim Quill"));

    // A quote of a message that is here.
    let question = "Are we still on for Saturday?";
    let asked = incoming(&key, &peer, &local(0), at(0), question);
    let mut answer = own(&key, &local(2), at(2), "Yes, ten at the usual place.");
    answer.reply_to = Some(quote(&key, &asked, text_of(question)));
    events.push(BackendEvent::Message(asked));
    events.extend(sent_and_read(&key, &peer, answer));

    // A photo whose caption its author edited, and our reply that quotes the photo.
    let mut photo = incoming(&key, &peer, &local(5), at(5), "");
    photo.body = Body::Media {
        attachments: vec![crate::attachments::synthetic_image(640, 400, 0x51_0e)],
        caption: Some(Text::plain("The view from the ridge")),
    };
    let edited = "The view from the ridge, just before the fog came in";
    let edit = BackendEvent::Edited {
        target: MsgTarget::to_id(Some(key.clone()), &photo.net_id()),
        by: Some(peer.clone()),
        new_local: Some(local(6).into()),
        body: Body::Text(Text::plain(edited)),
        edited_at: at(6),
        keeps_quote: None,
    };
    let mut praise = own(&key, &local(8), at(8), "Lovely light.");
    praise.reply_to = Some(quote(
        &key,
        &photo,
        QuoteSnapshot {
            text: Some(Text::plain(edited)),
            attachment: Some(AttachmentKind::Image),
            mime: Some("image/jpeg".into()),
            ..QuoteSnapshot::default()
        },
    ));
    events.push(BackendEvent::Message(photo));
    events.push(edit);
    events.extend(sent_and_read(&key, &peer, praise));

    // A quote of a message this device never had.
    let mut found = incoming(&key, &peer, &local(12), at(12), "Found them, by the way.");
    found.reply_to = Some(ReplyTo {
        target: MsgTarget {
            conv: Some(key.clone()),
            author: kourier_model::TargetAuthor::Own,
            local: (t0 - 400 * 24 * 3_600_000).to_string().into(),
            alternates: Vec::new(),
        },
        author: None,
        snapshot: Some(text_of("Can you bring the tent poles?")),
    });
    events.push(BackendEvent::Message(found));

    // A quote of a message its author deleted afterwards.
    let slip = "Ignore that, wrong chat.";
    let mistaken = incoming(&key, &peer, &local(15), at(15), slip);
    let mut shrug = own(&key, &local(16), at(16), "No worries.");
    shrug.reply_to = Some(quote(&key, &mistaken, text_of(slip)));
    let deleted = BackendEvent::Deleted {
        target: MsgTarget::to_id(Some(key.clone()), &mistaken.net_id()),
        by: Some(peer.clone()),
        for_everyone: true,
        at: at(17),
    };
    events.push(BackendEvent::Message(mistaken));
    events.extend(sent_and_read(&key, &peer, shrug));
    events.push(deleted);

    // ---- a chat with a disappearing-message timer ----
    let key = showcase_timer()?;
    let peer = fixture_user(0x1002)?;
    events.extend(chat(&key, &peer, "Rue Lumen"));
    let proposal = own(
        &key,
        &local(20),
        at(20),
        "Shall we turn on disappearing messages here?",
    );
    events.extend(sent_and_read(&key, &peer, proposal));
    let agreed = incoming(&key, &peer, &local(21), at(21), "Good idea, one week.");
    events.push(BackendEvent::Message(agreed));
    // A timer change is the patch (with the network's timer version) and its timeline row
    // (§4.2 `ChangeConversation`).
    let timer_change = |minutes: i64, timer: Duration, version: u32| {
        let mut patch = ConversationPatch::new(key.clone());
        patch.timer = Patch::Set(timer);
        patch.timer_version = Some(version);
        let mut notice = incoming(&key, &peer, &local(minutes), at(minutes), "");
        notice.body = Body::System(SystemEvent::TimerChanged {
            by: Some(peer.clone()),
            timer: Some(timer),
        });
        [
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(notice),
        ]
    };
    events.extend(timer_change(22, SHOWCASE_TIMER, 2));
    // Incoming messages count from when they are read: one read already, one still unread.
    // (Own messages would count from their send, long past on a real clock.)
    for (minutes, text, read) in [
        (24, "This one is gone in a week.", true),
        (25, "And this one once you have read it.", false),
    ] {
        let mut m = incoming(&key, &peer, &local(minutes), at(minutes), text);
        m.read = read;
        m.expiry = Some(Expiry {
            kind: DisappearKind::AfterRead,
            timer: SHOWCASE_TIMER,
            started_at: None,
        });
        events.push(BackendEvent::Message(m));
    }
    // The newest row is a timer notice, so the list has an event to describe.
    events.extend(timer_change(30, Duration::from_secs(86_400), 3));
    Ok(events)
}

/// A showcase contact and their direct chat with Signal's features.
fn chat(key: &ConvKey, peer: &UserId, name: &str) -> [BackendEvent; 2] {
    let mut contact = ContactPatch::new(peer.clone());
    contact.profile_name = Patch::Set(name.into());
    let mut patch = ConversationPatch::new(key.clone());
    patch.features = Some(signal_features(false));
    [
        BackendEvent::ContactUpsert(contact),
        BackendEvent::ConversationUpsert(patch),
    ]
}

/// A message from `peer` that we have read.
fn incoming(
    key: &ConvKey,
    peer: &UserId,
    local: &str,
    sent_at: i64,
    text: &str,
) -> IncomingMessage {
    let mut m = remote(key, peer, local, sent_at, text);
    m.read = true;
    m
}

/// An own message with what the fixture's own messages have: sent to `peer` and read by them.
fn sent_and_read(key: &ConvKey, peer: &UserId, m: IncomingMessage) -> [BackendEvent; 3] {
    let target = MsgTarget::to_id(Some(key.clone()), &m.net_id());
    let read_at = m.sent_at + 60_000;
    [
        BackendEvent::Message(m),
        BackendEvent::OwnSendState {
            local: None,
            target: target.clone(),
            per_recipient: vec![(peer.clone(), RecipientState::Sent { sealed: true })],
        },
        BackendEvent::Receipts {
            targets: vec![target],
            by: peer.clone(),
            kind: ReceiptKind::Read,
            at: read_at,
        },
    ]
}

/// A quote of `of` in `key` with the sender's `snapshot`.
fn quote(key: &ConvKey, of: &IncomingMessage, snapshot: QuoteSnapshot) -> ReplyTo {
    ReplyTo {
        target: MsgTarget::to_id(Some(key.clone()), &of.net_id()),
        author: of.origin.author().cloned(),
        snapshot: Some(snapshot),
    }
}
