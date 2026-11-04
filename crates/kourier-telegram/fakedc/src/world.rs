//! The fake DC's world (TD-4a C4a): the account's peers and chats, their messages, and the update
//! state TDLib follows. Private chats and basic groups share ONE message box: one `pts` and one
//! message id sequence; every channel (supergroup or broadcast) has its own `pts` and its own ids.
//! `qts`, `seq` and `date` are the common state's other fields.
//!
//! Every change is stored with the pts it ends at (`pts`, `pts_count`), whether it is pushed or only
//! queued (that is the push module's choice, C4b), so a difference from pts N holds exactly the
//! changes after N, in pts order. The slice limit and the too-long thresholds are the fake DC's
//! MODEL choices (P-5: a recording's behaviour that depends on them is never a golden fact).
//! Channel reads move no pts (`updateReadChannelInbox` carries the channel's pts, no `pts_count`):
//! they change the chat's state only.
mod answers;
mod json;

pub use answers::{CALL_ACCESS_HASH, HistoryQuery, Service};

use crate::tlb;
use grammers_tl_types as tl;
use std::collections::{BTreeMap, BTreeSet};

/// The account the fake DC logs in.
pub const OWN_USER_ID: i64 = 100;
/// The pts count a common difference holds at most when the request names no `pts_total_limit`.
pub const DEFAULT_SLICE_LIMIT: i32 = 100;
/// A common difference over more pts than this is `updates.differenceTooLong`.
pub const DEFAULT_TOO_LONG_AFTER: i32 = 1000;
/// A channel difference over more pts than this is `updates.channelDifferenceTooLong`.
pub const DEFAULT_CHANNEL_TOO_LONG_AFTER: i32 = 1000;

/// A chat of the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChatId {
    /// A private chat with this user.
    User(i64),
    /// A basic group.
    Basic(i64),
    /// A supergroup or a broadcast channel.
    Channel(i64),
}

impl ChatId {
    fn channel(self) -> Option<i64> {
        match self {
            ChatId::Channel(c) => Some(c),
            _ => None,
        }
    }

    pub fn peer(self) -> tl::enums::Peer {
        match self {
            ChatId::User(user_id) => tlb::peer_user(user_id),
            ChatId::Basic(chat_id) => tl::enums::Peer::Chat(tl::types::PeerChat { chat_id }),
            ChatId::Channel(channel_id) => {
                tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id })
            }
        }
    }
}

/// One message as the server holds it (`from` is [`OWN_USER_ID`] for an outgoing one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Msg {
    pub id: i32,
    pub from: i64,
    pub date: i32,
    pub text: String,
    pub edit_date: Option<i32>,
    /// A service message (a call line, a migration) instead of a text.
    pub service: Option<Service>,
}

/// What a chat is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Private,
    Basic {
        title: String,
    },
    Super {
        title: String,
        username: Option<String>,
    },
    Broadcast {
        title: String,
        username: Option<String>,
    },
}

/// A chat's state.
#[derive(Clone, Debug)]
pub struct Chat {
    pub kind: Kind,
    pub archived: bool,
    pub msgs: BTreeMap<i32, Msg>,
    pub read_inbox_max_id: i32,
    pub read_outbox_max_id: i32,
    /// A basic group that was upgraded: the supergroup it became.
    pub migrated_to: Option<i64>,
    /// A supergroup made from a basic group: that group and its last message id.
    pub migrated_from: Option<(i64, i32)>,
}

impl Chat {
    /// Incoming messages newer than the inbox read mark.
    pub fn unread_count(&self) -> i32 {
        let n = self
            .msgs
            .values()
            .filter(|m| m.from != OWN_USER_ID && m.id > self.read_inbox_max_id)
            .count();
        i32::try_from(n).unwrap_or(i32::MAX)
    }
}

/// A user of the world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub first: String,
    pub phone: String,
}

/// One change of a message box.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    New(ChatId, Msg),
    Edit(ChatId, Msg),
    Delete(ChatId, Vec<i32>),
    /// The inbox read up to `max_id`; `still_unread` incoming messages are left.
    ReadInbox {
        chat: ChatId,
        max_id: i32,
        still_unread: i32,
    },
    ReadOutbox {
        chat: ChatId,
        max_id: i32,
    },
}

impl Change {
    fn chat(&self) -> ChatId {
        match self {
            Change::New(c, _) | Change::Edit(c, _) | Change::Delete(c, _) => *c,
            Change::ReadInbox { chat, .. } | Change::ReadOutbox { chat, .. } => *chat,
        }
    }
}

/// A change with the pts it ends at: it covers `pts - pts_count + 1 ..= pts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged {
    pub pts: i32,
    pub pts_count: i32,
    pub date: i32,
    pub change: Change,
}

/// One message box: the common one or a channel's.
#[derive(Clone, Debug)]
struct MsgBox {
    pts: i32,
    next_id: i32,
    log: Vec<Logged>,
    too_long_after: i32,
}

impl MsgBox {
    fn new(too_long_after: i32) -> MsgBox {
        MsgBox {
            pts: 1,
            next_id: 1,
            log: Vec::new(),
            too_long_after,
        }
    }

    fn record(&mut self, pts_count: i32, date: i32, change: Change) -> Logged {
        self.pts += pts_count;
        let l = Logged {
            pts: self.pts,
            pts_count,
            date,
            change,
        };
        self.log.push(l.clone());
        l
    }
}

/// The changes after `from_pts` (in pts order), cut so that their pts counts add up to at most
/// `limit` (always at least one change); `true` when more changes follow the cut.
fn changes_after(log: &[Logged], from_pts: i32, limit: i32) -> (&[Logged], bool) {
    let start = log.partition_point(|l| l.pts <= from_pts);
    let rest = &log[start..];
    let (mut used, mut n) = (0, 0);
    for l in rest {
        if n > 0 && used + l.pts_count > limit {
            break;
        }
        used += l.pts_count;
        n += 1;
    }
    (&rest[..n], n < rest.len())
}

/// A common difference, before it becomes TL.
#[derive(Debug, PartialEq, Eq)]
pub enum Diff<'a> {
    Empty,
    TooLong,
    /// `slice`: more changes follow; the state to go on from ends at the last entry's pts.
    Changes {
        entries: &'a [Logged],
        slice: bool,
    },
}

/// A channel difference, before it becomes TL.
#[derive(Debug, PartialEq, Eq)]
pub enum ChannelDiff<'a> {
    /// The world holds no such channel.
    Unknown,
    Empty {
        pts: i32,
    },
    TooLong,
    /// `pts` is where the client goes on from; `last` false when more changes follow.
    Changes {
        entries: &'a [Logged],
        last: bool,
        pts: i32,
    },
}

/// The world: users, chats, the common box and the channel boxes.
#[derive(Clone, Debug)]
pub struct World {
    pub date: i32,
    pub seq: i32,
    pub qts: i32,
    pub slice_limit: i32,
    /// The common box's too-long threshold (`threshold common <pts>` sets it).
    pub too_long_after: i32,
    /// MODEL "a request lost in flight" (`hold-reads on`): while true a read request of the account
    /// is neither applied nor answered, and is gone for good (never applied later).
    pub hold_reads: bool,
    common: MsgBox,
    channels: BTreeMap<i64, MsgBox>,
    pub users: BTreeMap<i64, Person>,
    pub chats: BTreeMap<ChatId, Chat>,
    /// The QR login token was scanned (`scan`): the next `auth.exportLoginToken` succeeds.
    pub qr_scanned: bool,
    /// The calls the world started (`phoneCallRequested`), by id: the caller.
    pub calls: BTreeMap<i64, i64>,
    /// Every text the world ever served (also of edited or deleted messages), for `world.json`.
    pub served_texts: BTreeSet<String>,
}

impl World {
    /// The account alone: pts 1, qts 0, seq 1, no chats.
    pub fn new(date: i32) -> World {
        let mut users = BTreeMap::new();
        users.insert(
            OWN_USER_ID,
            Person {
                first: "Fake Self".into(),
                phone: String::new(),
            },
        );
        World {
            date,
            seq: 1,
            qts: 0,
            slice_limit: DEFAULT_SLICE_LIMIT,
            too_long_after: DEFAULT_TOO_LONG_AFTER,
            hold_reads: false,
            common: MsgBox::new(DEFAULT_TOO_LONG_AFTER),
            channels: BTreeMap::new(),
            users,
            chats: BTreeMap::new(),
            qr_scanned: false,
            calls: BTreeMap::new(),
            served_texts: BTreeSet::new(),
        }
    }

    /// The world a scenario names (`world <name>` on the control socket): `alone` (the account
    /// only) or `standard` ([`World::standard`]). The account's phone is kept.
    pub fn named(name: &str, date: i32, own_phone: &str) -> Option<World> {
        let mut w = match name {
            "alone" => World::new(date),
            "standard" => World::standard(date),
            "many-channels" => World::many_channels(date, true),
            "many-channels-unread" => World::many_channels(date, false),
            _ => return None,
        };
        w.set_own_phone(own_phone);
        Some(w)
    }

    /// The account's phone digits ("" before a sign-in).
    pub fn own_phone(&self) -> &str {
        self.users
            .get(&OWN_USER_ID)
            .map_or("", |p| p.phone.as_str())
    }

    /// The brief's world (§3.2): three peers with a private chat each (the third archived), one
    /// basic group, one supergroup, one public supergroup, one broadcast channel; one private chat
    /// with three unread messages, the supergroup with one. Every name and text is synthetic.
    pub fn standard(date: i32) -> World {
        let mut w = World::new(date);
        // Distinct dates (one second apart, older than `date`): the dialog order and its paging
        // follow them, as on the server.
        w.date = date - 100;
        for (id, first, phone) in [
            (201, "Peer Alpha", "999060000201"),
            (202, "Peer Bravo", "999060000202"),
            (203, "Peer Charlie", "999060000203"),
        ] {
            w.add_user(id, first, phone);
            w.add_chat(ChatId::User(id), Kind::Private, id == 203);
        }
        w.add_chat(
            ChatId::Basic(301),
            Kind::Basic {
                title: "Group Delta".into(),
            },
            false,
        );
        w.add_chat(
            ChatId::Channel(401),
            Kind::Super {
                title: "Super Echo".into(),
                username: None,
            },
            false,
        );
        w.add_chat(
            ChatId::Channel(402),
            Kind::Super {
                title: "Public Foxtrot".into(),
                username: Some("publicfoxtrot".into()),
            },
            false,
        );
        w.add_chat(
            ChatId::Channel(403),
            Kind::Broadcast {
                title: "Channel Golf".into(),
                username: Some("channelgolf".into()),
            },
            false,
        );
        w.new_message(ChatId::User(201), OWN_USER_ID, "hello alpha");
        for text in ["alpha one", "alpha two", "alpha three"] {
            w.new_message(ChatId::User(201), 201, text);
        }
        let b = w.new_message(ChatId::User(202), 202, "bravo one");
        if let Change::New(_, m) = &b.change {
            let id = m.id;
            w.read_inbox(ChatId::User(202), id);
        }
        w.new_message(ChatId::User(203), 203, "charlie one");
        w.new_message(ChatId::Basic(301), 201, "delta one");
        w.new_message(ChatId::Basic(301), OWN_USER_ID, "delta two");
        w.new_message(ChatId::Channel(401), 202, "echo one");
        w.new_message(ChatId::Channel(402), 203, "foxtrot one");
        w.new_message(ChatId::Channel(403), 0, "golf one");
        w.date = date;
        w
    }

    /// The perf cases' world: one peer and fifty supergroups (`c1000`..`c1049`) with one read message
    /// each, or (`read` false) three unread.
    pub fn many_channels(date: i32, read: bool) -> World {
        let mut w = World::new(date);
        w.date = date - 100;
        w.add_user(201, "Peer Alpha", "999060000201");
        w.add_chat(ChatId::User(201), Kind::Private, false);
        // (a listed chat without a last message never ends TDLib's paging: `on_get_dialogs`)
        let first = w.new_message(ChatId::User(201), OWN_USER_ID, "hello");
        if let Change::New(_, m) = &first.change {
            let id = m.id;
            w.read_inbox(ChatId::User(201), id);
        }
        for i in 0..50 {
            let chat = ChatId::Channel(1000 + i);
            w.add_chat(
                chat,
                Kind::Super {
                    title: format!("Super {i}"),
                    username: None,
                },
                false,
            );
            let logged = w.new_message(chat, 201, "hello");
            if let (true, Change::New(_, m)) = (read, &logged.change) {
                let id = m.id;
                w.read_inbox(chat, id);
            } else {
                // More than the last message: the start sync's history fetch for this chat.
                w.new_message(chat, 201, "hello again");
                w.new_message(chat, 201, "hello once more");
            }
        }
        w.date = date;
        w
    }

    pub fn add_user(&mut self, id: i64, first: &str, phone: &str) {
        self.users.insert(
            id,
            Person {
                first: first.into(),
                phone: phone.into(),
            },
        );
    }

    pub fn add_chat(&mut self, id: ChatId, kind: Kind, archived: bool) {
        if let Some(c) = id.channel() {
            self.channels
                .insert(c, MsgBox::new(DEFAULT_CHANNEL_TOO_LONG_AFTER));
        }
        self.chats.insert(
            id,
            Chat {
                kind,
                archived,
                msgs: BTreeMap::new(),
                read_inbox_max_id: 0,
                read_outbox_max_id: 0,
                migrated_to: None,
                migrated_from: None,
            },
        );
    }

    /// The account's own phone (the digits the sign-in named).
    pub fn set_own_phone(&mut self, phone: &str) {
        if let Some(p) = self.users.get_mut(&OWN_USER_ID) {
            p.phone = phone.into();
        }
    }

    pub fn pts(&self) -> i32 {
        self.common.pts
    }

    pub fn channel_pts(&self, channel: i64) -> Option<i32> {
        self.channels.get(&channel).map(|b| b.pts)
    }

    /// The channel's too-long threshold (RC6's second case sets it low).
    pub fn set_channel_too_long_after(&mut self, channel: i64, after: i32) {
        if let Some(b) = self.channels.get_mut(&channel) {
            b.too_long_after = after;
        }
    }

    fn msg_box(&mut self, chat: ChatId) -> &mut MsgBox {
        match chat.channel() {
            Some(c) => self
                .channels
                .entry(c)
                .or_insert_with(|| MsgBox::new(DEFAULT_CHANNEL_TOO_LONG_AFTER)),
            None => &mut self.common,
        }
    }

    /// A new message in `chat` from `from` (0: a broadcast channel's post). Panics on an unknown
    /// chat: the world's scenarios name only chats they made.
    pub fn new_message(&mut self, chat: ChatId, from: i64, text: &str) -> Logged {
        self.new_any(chat, from, text, None)
    }

    /// A new service message in `chat` (a call line, a migration's line).
    pub fn new_service(&mut self, chat: ChatId, from: i64, service: Service) -> Logged {
        self.new_any(chat, from, "", Some(service))
    }

    fn new_any(&mut self, chat: ChatId, from: i64, text: &str, service: Option<Service>) -> Logged {
        assert!(self.chats.contains_key(&chat), "no chat {chat:?}");
        if self
            .chats
            .get(&chat)
            .is_some_and(|c| c.migrated_to.is_some())
        {
            panic!("chat {chat:?} was upgraded");
        }
        // Every new message is one second later than the one before (dialog order, paging).
        self.date += 1;
        let date = self.date;
        if service.is_none() {
            self.served_texts.insert(text.to_string());
        }
        let b = self.msg_box(chat);
        let id = b.next_id;
        b.next_id += 1;
        let msg = Msg {
            id,
            from,
            date,
            text: text.into(),
            edit_date: None,
            service,
        };
        let l = b.record(1, date, Change::New(chat, msg.clone()));
        let c = self.chats.get_mut(&chat).expect("checked");
        if from == OWN_USER_ID {
            // Sending reads the chat up to the sent message.
            c.read_inbox_max_id = c.read_inbox_max_id.max(id);
        }
        c.msgs.insert(id, msg);
        l
    }

    /// A new text for message `id` of `chat`; `None` when the world has no such message.
    pub fn edit_message(&mut self, chat: ChatId, id: i32, text: &str) -> Option<Logged> {
        let date = self.date;
        let m = self.chats.get_mut(&chat)?.msgs.get_mut(&id)?;
        if m.service.is_some() {
            return None;
        }
        self.served_texts.insert(text.to_string());
        m.text = text.into();
        m.edit_date = Some(date);
        let m = m.clone();
        Some(self.msg_box(chat).record(1, date, Change::Edit(chat, m)))
    }

    /// Deletes the listed messages of `chat` that exist (one pts each); `None` when none does.
    pub fn delete_messages(&mut self, chat: ChatId, ids: &[i32]) -> Option<Logged> {
        let date = self.date;
        let c = self.chats.get_mut(&chat)?;
        let gone: Vec<i32> = ids
            .iter()
            .copied()
            .filter(|id| c.msgs.remove(id).is_some())
            .collect();
        if gone.is_empty() {
            return None;
        }
        let n = i32::try_from(gone.len()).unwrap_or(i32::MAX);
        Some(
            self.msg_box(chat)
                .record(n, date, Change::Delete(chat, gone)),
        )
    }

    /// The account read `chat` up to `max_id` (on another device). A channel's read moves no pts:
    /// the result is the change at the channel's CURRENT pts, not stored.
    pub fn read_inbox(&mut self, chat: ChatId, max_id: i32) -> Option<Logged> {
        let date = self.date;
        let c = self.chats.get_mut(&chat)?;
        c.read_inbox_max_id = c.read_inbox_max_id.max(max_id);
        let change = Change::ReadInbox {
            chat,
            max_id,
            still_unread: c.unread_count(),
        };
        Some(self.pts_change(chat, date, change))
    }

    /// The peer read the account's messages in `chat` up to `max_id`.
    pub fn read_outbox(&mut self, chat: ChatId, max_id: i32) -> Option<Logged> {
        let date = self.date;
        let c = self.chats.get_mut(&chat)?;
        c.read_outbox_max_id = c.read_outbox_max_id.max(max_id);
        Some(self.pts_change(chat, date, Change::ReadOutbox { chat, max_id }))
    }

    /// The account's own read request (`messages.readHistory`, `channels.readHistory`, TD-4 K1):
    /// the inbox read mark of `chat` becomes EXACTLY `max_id` when that raises it (MODEL: the fake
    /// stores what the request names; a real server may clamp it), through [`World::read_inbox`]
    /// (a common-box chat: one pts, the change stored for a difference; a channel: no pts, not
    /// stored). A read at or below the mark changes nothing and moves no pts (`pts_count` 0 at the
    /// box's current pts). The caller does not push the change: the reading session gets the
    /// request's answer only (MODEL: a real server tells the account's OTHER sessions).
    pub fn read_by_self(&mut self, chat: ChatId, max_id: i32) -> Option<Logged> {
        let date = self.date;
        let c = self.chats.get(&chat)?;
        if max_id > c.read_inbox_max_id {
            return self.read_inbox(chat, max_id);
        }
        let change = Change::ReadInbox {
            chat,
            max_id: c.read_inbox_max_id,
            still_unread: c.unread_count(),
        };
        Some(self.unstored(chat, date, change))
    }

    /// The inbox and outbox read marks of `chat` (`read-state <chat>`).
    pub fn read_state(&self, chat: ChatId) -> Option<(i32, i32)> {
        let c = self.chats.get(&chat)?;
        Some((c.read_inbox_max_id, c.read_outbox_max_id))
    }

    fn pts_change(&mut self, chat: ChatId, date: i32, change: Change) -> Logged {
        if chat.channel().is_some() {
            self.unstored(chat, date, change)
        } else {
            self.msg_box(chat).record(1, date, change)
        }
    }

    /// A change at its box's CURRENT pts with no pts count, not stored (a channel's read moves no
    /// pts; a read that changed nothing).
    fn unstored(&mut self, chat: ChatId, date: i32, change: Change) -> Logged {
        let b = self.msg_box(chat);
        Logged {
            pts: b.pts,
            pts_count: 0,
            date,
            change,
        }
    }

    /// Unread incoming messages over every chat.
    pub fn unread_count(&self) -> i32 {
        self.chats.values().map(Chat::unread_count).sum()
    }

    fn state_at(&self, pts: i32) -> tl::enums::updates::State {
        tl::enums::updates::State::State(tl::types::updates::State {
            pts,
            qts: self.qts,
            date: self.date,
            seq: self.seq,
            unread_count: self.unread_count(),
        })
    }

    /// `updates.getState`.
    pub fn state(&self) -> tl::enums::updates::State {
        self.state_at(self.common.pts)
    }

    /// The common difference from `from_pts` (`limit`: the request's `pts_total_limit`).
    pub fn difference(&self, from_pts: i32, limit: Option<i32>) -> Diff<'_> {
        if from_pts >= self.common.pts {
            return Diff::Empty;
        }
        if self.common.pts - from_pts > self.too_long_after {
            return Diff::TooLong;
        }
        let limit = limit.unwrap_or(self.slice_limit).max(1);
        let (entries, slice) = changes_after(&self.common.log, from_pts, limit);
        Diff::Changes { entries, slice }
    }

    /// `updates.getDifference` as TL.
    pub fn difference_tl(
        &self,
        from_pts: i32,
        limit: Option<i32>,
    ) -> tl::enums::updates::Difference {
        use tl::enums::updates::Difference as D;
        match self.difference(from_pts, limit) {
            Diff::Empty => D::Empty(tl::types::updates::DifferenceEmpty {
                date: self.date,
                seq: self.seq,
            }),
            Diff::TooLong => D::TooLong(tl::types::updates::DifferenceTooLong {
                pts: self.common.pts,
            }),
            Diff::Changes { entries, slice } => {
                let (new_messages, other_updates) = self.split(entries);
                let (chats, users) = self.peers_of(entries);
                if slice {
                    let at = entries.last().map_or(from_pts, |l| l.pts);
                    D::Slice(tl::types::updates::DifferenceSlice {
                        new_messages,
                        new_encrypted_messages: Vec::new(),
                        other_updates,
                        chats,
                        users,
                        intermediate_state: self.state_at(at),
                    })
                } else {
                    D::Difference(tl::types::updates::Difference {
                        new_messages,
                        new_encrypted_messages: Vec::new(),
                        other_updates,
                        chats,
                        users,
                        state: self.state(),
                    })
                }
            }
        }
    }

    /// The difference of channel `channel` from `from_pts`, at most `limit` pts.
    pub fn channel_difference(&self, channel: i64, from_pts: i32, limit: i32) -> ChannelDiff<'_> {
        let Some(b) = self.channels.get(&channel) else {
            return ChannelDiff::Unknown;
        };
        if from_pts >= b.pts {
            return ChannelDiff::Empty { pts: b.pts };
        }
        if b.pts - from_pts > b.too_long_after {
            return ChannelDiff::TooLong;
        }
        let (entries, more) = changes_after(&b.log, from_pts, limit.max(1));
        let pts = if more {
            entries.last().map_or(from_pts, |l| l.pts)
        } else {
            b.pts
        };
        ChannelDiff::Changes {
            entries,
            last: !more,
            pts,
        }
    }

    /// `updates.getChannelDifference` as TL (too long: the dialog and the latest messages, C4b);
    /// `Err` names the rpc error.
    pub fn channel_difference_tl(
        &self,
        channel: i64,
        from_pts: i32,
        limit: i32,
    ) -> Result<tl::enums::updates::ChannelDifference, &'static str> {
        use tl::enums::updates::ChannelDifference as D;
        match self.channel_difference(channel, from_pts, limit) {
            ChannelDiff::Unknown => Err("CHANNEL_INVALID"),
            ChannelDiff::TooLong => self.channel_too_long_tl(channel).ok_or("CHANNEL_INVALID"),
            ChannelDiff::Empty { pts } => {
                Ok(D::Empty(tl::types::updates::ChannelDifferenceEmpty {
                    r#final: true,
                    pts,
                    timeout: None,
                }))
            }
            ChannelDiff::Changes { entries, last, pts } => {
                let (new_messages, other_updates) = self.split(entries);
                let (chats, users) = self.peers_of(entries);
                Ok(D::Difference(tl::types::updates::ChannelDifference {
                    r#final: last,
                    pts,
                    timeout: None,
                    new_messages,
                    other_updates,
                    chats,
                    users,
                }))
            }
        }
    }

    /// New messages go into `new_messages`, everything else into `other_updates`.
    fn split(&self, entries: &[Logged]) -> (Vec<tl::enums::Message>, Vec<tl::enums::Update>) {
        let mut msgs = Vec::new();
        let mut others = Vec::new();
        for l in entries {
            match &l.change {
                Change::New(chat, m) => msgs.push(self.message_tl(*chat, m)),
                _ => others.push(self.update_of(l)),
            }
        }
        (msgs, others)
    }

    /// The TL message of `m` in `chat`.
    pub fn message_tl(&self, chat: ChatId, m: &Msg) -> tl::enums::Message {
        let post = matches!(
            self.chats.get(&chat).map(|c| &c.kind),
            Some(Kind::Broadcast { .. })
        );
        let parts = tlb::MessageParts {
            id: m.id,
            peer: chat.peer(),
            from: (!post).then_some(m.from),
            out: m.from == OWN_USER_ID,
            post,
            date: m.date,
            text: &m.text,
            edit_date: m.edit_date,
        };
        match &m.service {
            Some(s) => tlb::service_message(parts, s.action_tl()),
            None => tlb::message(parts),
        }
    }

    /// The long form of one change (the update a push or a difference carries).
    pub fn update_of(&self, l: &Logged) -> tl::enums::Update {
        use tl::enums::Update as U;
        use tl::types as t;
        let (pts, pts_count) = (l.pts, l.pts_count);
        match (&l.change, l.change.chat().channel()) {
            (Change::New(chat, m), None) => U::NewMessage(t::UpdateNewMessage {
                message: self.message_tl(*chat, m),
                pts,
                pts_count,
            }),
            (Change::New(chat, m), Some(_)) => U::NewChannelMessage(t::UpdateNewChannelMessage {
                message: self.message_tl(*chat, m),
                pts,
                pts_count,
            }),
            (Change::Edit(chat, m), None) => U::EditMessage(t::UpdateEditMessage {
                message: self.message_tl(*chat, m),
                pts,
                pts_count,
            }),
            (Change::Edit(chat, m), Some(_)) => {
                U::EditChannelMessage(t::UpdateEditChannelMessage {
                    message: self.message_tl(*chat, m),
                    pts,
                    pts_count,
                })
            }
            (Change::Delete(_, ids), None) => U::DeleteMessages(t::UpdateDeleteMessages {
                messages: ids.clone(),
                pts,
                pts_count,
            }),
            (Change::Delete(_, ids), Some(channel_id)) => {
                U::DeleteChannelMessages(t::UpdateDeleteChannelMessages {
                    channel_id,
                    messages: ids.clone(),
                    pts,
                    pts_count,
                })
            }
            (
                Change::ReadInbox {
                    chat,
                    max_id,
                    still_unread,
                },
                None,
            ) => U::ReadHistoryInbox(t::UpdateReadHistoryInbox {
                folder_id: None,
                peer: chat.peer(),
                top_msg_id: None,
                max_id: *max_id,
                still_unread_count: *still_unread,
                pts,
                pts_count,
            }),
            (
                Change::ReadInbox {
                    max_id,
                    still_unread,
                    ..
                },
                Some(channel_id),
            ) => U::ReadChannelInbox(t::UpdateReadChannelInbox {
                folder_id: None,
                channel_id,
                max_id: *max_id,
                still_unread_count: *still_unread,
                pts,
            }),
            (Change::ReadOutbox { chat, max_id }, None) => {
                U::ReadHistoryOutbox(t::UpdateReadHistoryOutbox {
                    peer: chat.peer(),
                    max_id: *max_id,
                    pts,
                    pts_count,
                })
            }
            (Change::ReadOutbox { max_id, .. }, Some(channel_id)) => {
                U::ReadChannelOutbox(t::UpdateReadChannelOutbox {
                    channel_id,
                    max_id: *max_id,
                })
            }
        }
    }

    /// The chats and users the entries name (their chats, the senders), in id order.
    pub fn peers_of(&self, entries: &[Logged]) -> (Vec<tl::enums::Chat>, Vec<tl::enums::User>) {
        let mut chats = BTreeSet::new();
        let mut users = BTreeSet::new();
        for l in entries {
            let chat = l.change.chat();
            match chat {
                ChatId::User(u) => {
                    users.insert(u);
                }
                ChatId::Basic(_) | ChatId::Channel(_) => {
                    chats.insert(chat);
                }
            }
            if let Change::New(_, m) | Change::Edit(_, m) = &l.change
                && self.users.contains_key(&m.from)
            {
                users.insert(m.from);
            }
        }
        let chats = chats.into_iter().filter_map(|c| self.chat_tl(c)).collect();
        let users = users.into_iter().filter_map(|u| self.user_tl(u)).collect();
        (chats, users)
    }

    pub fn user_tl(&self, id: i64) -> Option<tl::enums::User> {
        let p = self.users.get(&id)?;
        Some(tlb::user(id, &p.first, &p.phone, id == OWN_USER_ID))
    }

    /// A group's or channel's TL chat (`None` for a private chat or an unknown one).
    pub fn chat_tl(&self, id: ChatId) -> Option<tl::enums::Chat> {
        let c = self.chats.get(&id)?;
        match (id, &c.kind) {
            (ChatId::Basic(chat), Kind::Basic { title }) => {
                Some(tlb::basic_chat(chat, title, 3, self.date, c.migrated_to))
            }
            (ChatId::Channel(ch), Kind::Super { title, username }) => Some(tlb::channel(
                ch,
                title,
                username.as_deref(),
                false,
                self.date,
            )),
            (ChatId::Channel(ch), Kind::Broadcast { title, username }) => Some(tlb::channel(
                ch,
                title,
                username.as_deref(),
                true,
                self.date,
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::{Deserializable, Serializable};

    const A: ChatId = ChatId::User(201);
    const G: ChatId = ChatId::Basic(301);
    const S: ChatId = ChatId::Channel(401);

    fn pts_of(entries: &[Logged]) -> Vec<i32> {
        entries.iter().map(|l| l.pts).collect()
    }

    #[test]
    fn a_fresh_world_is_at_pts_one_and_its_difference_is_empty() {
        let w = World::new(7);
        let tl::enums::updates::State::State(s) = w.state();
        assert_eq!(
            (s.pts, s.qts, s.seq, s.date, s.unread_count),
            (1, 0, 1, 7, 0)
        );
        assert_eq!(w.difference(1, None), Diff::Empty);
        assert!(matches!(
            w.difference_tl(1, None),
            tl::enums::updates::Difference::Empty(e) if e.date == 7 && e.seq == 1
        ));
    }

    #[test]
    fn private_chats_and_basic_groups_share_one_box_and_channels_have_their_own() {
        let mut w = World::standard(1);
        let common = w.pts();
        let ch = w.channel_pts(401).unwrap();
        let a = w.new_message(A, 201, "x");
        let g = w.new_message(G, 202, "y");
        let s = w.new_message(S, 202, "z");
        assert_eq!((a.pts, g.pts), (common + 1, common + 2));
        assert_eq!(s.pts, ch + 1);
        assert_eq!(w.channel_pts(402), Some(2), "another channel is untouched");
        let (Change::New(_, ma), Change::New(_, mg), Change::New(_, ms)) =
            (&a.change, &g.change, &s.change)
        else {
            panic!("new messages");
        };
        assert_eq!(mg.id, ma.id + 1, "one id sequence for the common box");
        assert_eq!(ms.id, 2, "the channel's own ids");
    }

    #[test]
    fn a_difference_from_pts_n_holds_exactly_the_changes_after_n() {
        let mut w = World::standard(1);
        let n = w.pts();
        let first = w.new_message(A, 201, "after one");
        w.new_message(G, 201, "after two");
        w.edit_message(A, 2, "edited").unwrap();
        w.delete_messages(A, &[3, 4, 999]).unwrap();
        w.read_outbox(A, 1).unwrap();
        w.new_message(S, 202, "a channel's own");
        let Diff::Changes { entries, slice } = w.difference(n, None) else {
            panic!("changes");
        };
        assert!(!slice);
        assert_eq!(
            entries.first(),
            Some(&first),
            "change n itself is not in it"
        );
        assert_eq!(pts_of(entries), [n + 1, n + 2, n + 3, n + 5, n + 6]);
        assert_eq!(entries[3].pts_count, 2, "two of the three ids existed");
        // From every point on, exactly the later changes.
        for from in n..w.pts() {
            let Diff::Changes { entries, .. } = w.difference(from, None) else {
                panic!("changes from {from}");
            };
            assert!(entries.iter().all(|l| l.pts > from));
            let sum: i32 = entries.iter().map(|l| l.pts_count).sum();
            let first_covers = entries[0].pts - entries[0].pts_count;
            assert_eq!(sum, w.pts() - first_covers.max(n));
        }
        assert_eq!(w.difference(w.pts(), None), Diff::Empty);
    }

    #[test]
    fn the_tl_difference_puts_messages_and_other_updates_apart_with_their_peers() {
        let mut w = World::standard(1);
        let n = w.pts();
        w.new_message(G, 202, "in the group");
        w.edit_message(A, 2, "edited").unwrap();
        let bytes = w.difference_tl(n, None).to_bytes();
        let tl::enums::updates::Difference::Difference(d) =
            tl::enums::updates::Difference::from_bytes(&bytes).unwrap()
        else {
            panic!("a difference");
        };
        assert_eq!(d.new_messages.len(), 1);
        let [tl::enums::Update::EditMessage(e)] = d.other_updates.as_slice() else {
            panic!("one edit");
        };
        assert_eq!((e.pts, e.pts_count), (n + 2, 1));
        let tl::enums::updates::State::State(s) = &d.state;
        assert_eq!(s.pts, w.pts());
        assert!(matches!(d.chats.as_slice(), [tl::enums::Chat::Chat(c)] if c.id == 301));
        let ids: Vec<i64> = d
            .users
            .iter()
            .map(|u| match u {
                tl::enums::User::User(u) => u.id,
                tl::enums::User::Empty(u) => u.id,
            })
            .collect();
        assert_eq!(ids, [201, 202]);
    }

    #[test]
    fn a_slice_stops_at_its_limit_and_goes_on_from_its_intermediate_state() {
        let mut w = World::new(1);
        w.add_user(201, "Peer Alpha", "999060000201");
        w.add_chat(A, Kind::Private, false);
        for i in 0..5 {
            w.new_message(A, 201, &format!("m{i}"));
        }
        let ids: Vec<i32> = w.chats[&A].msgs.keys().take(3).copied().collect();
        w.delete_messages(A, &ids).unwrap(); // pts 7..=9 in one change
        w.new_message(A, 201, "last");
        let Diff::Changes { entries, slice } = w.difference(1, Some(2)) else {
            panic!("changes");
        };
        assert!(slice);
        assert_eq!(pts_of(entries), [2, 3]);
        let Diff::Changes { entries, slice } = w.difference(5, Some(2)) else {
            panic!("changes");
        };
        assert!(slice);
        assert_eq!(
            pts_of(entries),
            [6],
            "the 3-pts delete does not fit in what is left"
        );
        let Diff::Changes { entries, slice } = w.difference(6, Some(2)) else {
            panic!("changes");
        };
        assert!(slice, "a change bigger than the limit comes alone");
        assert_eq!(pts_of(entries), [9]);
        let Diff::Changes { entries, slice } = w.difference(9, Some(2)) else {
            panic!("changes");
        };
        assert!(!slice);
        assert_eq!(pts_of(entries), [10]);
        // The TL slice names the pts to go on from.
        let tl::enums::updates::Difference::Slice(s) = w.difference_tl(1, Some(2)) else {
            panic!("a slice");
        };
        let tl::enums::updates::State::State(st) = s.intermediate_state;
        assert_eq!(st.pts, 3);
        // Without pts_total_limit the world's limit applies.
        w.slice_limit = 4;
        let Diff::Changes { entries, slice } = w.difference(1, None) else {
            panic!("changes");
        };
        assert!(slice);
        assert_eq!(pts_of(entries), [2, 3, 4, 5]);
    }

    #[test]
    fn a_difference_over_the_threshold_is_too_long() {
        let mut w = World::standard(1);
        w.too_long_after = 3;
        let n = w.pts();
        for i in 0..4 {
            w.new_message(A, 201, &format!("m{i}"));
        }
        assert_eq!(w.difference(n, None), Diff::TooLong);
        assert!(matches!(w.difference(n + 1, None), Diff::Changes { .. }));
        assert!(matches!(
            w.difference_tl(n, None),
            tl::enums::updates::Difference::TooLong(t) if t.pts == w.pts()
        ));
    }

    #[test]
    fn a_channel_difference_follows_the_channels_own_pts() {
        let mut w = World::standard(1);
        let p = w.channel_pts(401).unwrap();
        for i in 0..5 {
            w.new_message(S, 202, &format!("c{i}"));
        }
        w.new_message(A, 201, "not the channel's");
        let ChannelDiff::Changes { entries, last, pts } = w.channel_difference(401, p, 3) else {
            panic!("changes");
        };
        assert!(!last);
        assert_eq!((pts_of(entries), pts), (vec![p + 1, p + 2, p + 3], p + 3));
        let ChannelDiff::Changes { entries, last, pts } = w.channel_difference(401, pts, 3) else {
            panic!("changes");
        };
        assert!(last);
        assert_eq!((entries.len(), pts), (2, p + 5));
        assert_eq!(
            w.channel_difference(401, p + 5, 3),
            ChannelDiff::Empty { pts: p + 5 }
        );
        assert_eq!(w.channel_difference(499, 1, 3), ChannelDiff::Unknown);
        w.set_channel_too_long_after(401, 2);
        assert_eq!(w.channel_difference(401, p, 3), ChannelDiff::TooLong);
        let bytes = w.channel_difference_tl(401, p + 3, 10).unwrap().to_bytes();
        let tl::enums::updates::ChannelDifference::Difference(d) =
            tl::enums::updates::ChannelDifference::from_bytes(&bytes).unwrap()
        else {
            panic!("a channel difference");
        };
        assert!(d.r#final && d.pts == p + 5 && d.new_messages.len() == 2);
        assert!(matches!(d.chats.as_slice(), [tl::enums::Chat::Channel(c)] if c.id == 401));
    }

    #[test]
    fn reads_and_unread_counts() {
        let mut w = World::standard(1);
        assert_eq!(w.chats[&A].unread_count(), 3);
        assert_eq!(w.chats[&S].unread_count(), 1);
        let n = w.pts();
        let r = w.read_inbox(A, 3).unwrap();
        assert_eq!((r.pts, r.pts_count), (n + 1, 1));
        assert!(matches!(
            w.update_of(&r),
            tl::enums::Update::ReadHistoryInbox(u) if u.max_id == 3 && u.still_unread_count == 1
        ));
        // A channel's read moves no pts and is not in its difference.
        let cp = w.channel_pts(401).unwrap();
        let rc = w.read_inbox(S, 1).unwrap();
        assert_eq!((rc.pts, rc.pts_count), (cp, 0));
        assert_eq!(w.channel_pts(401), Some(cp));
        assert!(matches!(
            w.update_of(&rc),
            tl::enums::Update::ReadChannelInbox(u) if u.channel_id == 401 && u.pts == cp
        ));
        assert_eq!(w.chats[&S].unread_count(), 0);
        // Sending reads the chat.
        w.new_message(A, OWN_USER_ID, "reply");
        assert_eq!(w.chats[&A].unread_count(), 0);
    }

    #[test]
    fn updates_take_the_long_forms_of_their_box() {
        let mut w = World::standard(1);
        let s = w.new_message(S, 202, "c");
        assert!(matches!(
            w.update_of(&s),
            tl::enums::Update::NewChannelMessage(_)
        ));
        let d = w.delete_messages(S, &[1]).unwrap();
        assert!(matches!(
            w.update_of(&d),
            tl::enums::Update::DeleteChannelMessages(u) if u.channel_id == 401 && u.messages == [1]
        ));
        let p = w.new_message(ChatId::Channel(403), 0, "post");
        let tl::enums::Update::NewChannelMessage(u) = w.update_of(&p) else {
            panic!("a channel message");
        };
        let tl::enums::Message::Message(m) = u.message else {
            panic!("a message");
        };
        assert!(m.post && m.from_id.is_none() && !m.out);
        let o = w.new_message(A, OWN_USER_ID, "mine");
        let tl::enums::Update::NewMessage(u) = w.update_of(&o) else {
            panic!("a message");
        };
        let tl::enums::Message::Message(m) = u.message else {
            panic!("a message");
        };
        assert!(m.out);
        assert_eq!(
            m.peer_id,
            tlb::peer_user(201),
            "a private chat names the other user"
        );
        assert!(w.edit_message(A, 9999, "none").is_none());
        assert!(w.delete_messages(A, &[9999]).is_none());
    }
}
