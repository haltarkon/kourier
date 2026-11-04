//! The world's answers to TDLib's reads (TD-4a C4b): the dialog list and its pages, the pinned and
//! the named dialogs, a chat's history, messages by id, chats and full chats, and the too-long
//! channel difference (which carries a dialog). Also the world's changes that are not plain
//! messages: a call (requested, then missed) and the basic-group upgrade.
//!
//! The dialog order is the top message's date, newest first (then the chat id): the world gives
//! every message its own second, so the order and its pages are those of the server. The page
//! size is the request's `limit` (at most [`DIALOGS_LIMIT`]): a MODEL choice (P-5).

use super::{Change, ChatId, Kind, Logged, Msg, OWN_USER_ID, World};
use crate::tlb;
use grammers_tl_types as tl;
use std::collections::BTreeSet;

/// The most dialogs one `messages.getDialogs` answers (the server's own limit).
pub const DIALOGS_LIMIT: usize = 100;
/// The messages a too-long channel difference carries (the latest ones).
pub const TOO_LONG_MESSAGES: usize = 10;
/// The first call id the world hands out.
const FIRST_CALL_ID: i64 = 7_000_001;
/// The access hash of every call (TDLib sends it back in `inputPhoneCall`; with patch 0002 never).
pub const CALL_ACCESS_HASH: i64 = 42_424_242;

/// A service message's action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Service {
    /// The call line of a call that was not answered.
    MissedCall { call_id: i64 },
    /// The basic group's last line: it became this supergroup.
    ChatMigrateTo { channel_id: i64 },
    /// The supergroup's first line: it was this basic group.
    ChannelMigrateFrom { title: String, chat_id: i64 },
}

impl Service {
    pub fn action_tl(&self) -> tl::enums::MessageAction {
        use tl::enums::MessageAction as A;
        match self {
            Service::MissedCall { call_id } => A::PhoneCall(tl::types::MessageActionPhoneCall {
                video: false,
                call_id: *call_id,
                reason: Some(tl::enums::PhoneCallDiscardReason::Missed),
                duration: None,
            }),
            Service::ChatMigrateTo { channel_id } => {
                A::ChatMigrateTo(tl::types::MessageActionChatMigrateTo {
                    channel_id: *channel_id,
                })
            }
            Service::ChannelMigrateFrom { title, chat_id } => {
                A::ChannelMigrateFrom(tl::types::MessageActionChannelMigrateFrom {
                    title: title.clone(),
                    chat_id: *chat_id,
                })
            }
        }
    }
}

/// `messages.getHistory`'s paging fields.
#[derive(Clone, Copy, Debug, Default)]
pub struct HistoryQuery {
    pub offset_id: i32,
    pub add_offset: i32,
    pub limit: i32,
    pub max_id: i32,
    pub min_id: i32,
}

impl World {
    /// The chat an `InputPeer` names (`None`: empty, the account itself, or not in the world).
    pub fn chat_of_input(&self, peer: &tl::enums::InputPeer) -> Option<ChatId> {
        use tl::enums::InputPeer as P;
        let id = match peer {
            P::User(u) => ChatId::User(u.user_id),
            P::UserFromMessage(u) => ChatId::User(u.user_id),
            P::Chat(c) => ChatId::Basic(c.chat_id),
            P::Channel(c) => ChatId::Channel(c.channel_id),
            P::ChannelFromMessage(c) => ChatId::Channel(c.channel_id),
            P::Empty | P::PeerSelf => return None,
        };
        self.chats.contains_key(&id).then_some(id)
    }

    /// The newest message of `chat`.
    fn top(&self, chat: ChatId) -> Option<&Msg> {
        self.chats.get(&chat)?.msgs.values().next_back()
    }

    /// The dialogs of folder `folder` (0 the main list, 1 the archive), newest first.
    pub fn dialog_order(&self, folder: i32) -> Vec<ChatId> {
        let mut v: Vec<(i32, i32, ChatId)> = self
            .chats
            .iter()
            .filter(|(_, c)| i32::from(c.archived) == folder && c.migrated_to.is_none())
            .map(|(id, _)| {
                let (date, top) = self.top(*id).map_or((0, 0), |m| (m.date, m.id));
                (date, top, *id)
            })
            .collect();
        v.sort_by(|a, b| b.cmp(a));
        v.into_iter().map(|(_, _, id)| id).collect()
    }

    /// One page of `folder`'s dialogs after the offset (the offset peer when it is in the list,
    /// else the first dialog older than `offset_date`), at most `limit`; and the list's length.
    pub fn dialogs_page(
        &self,
        folder: i32,
        offset_date: i32,
        offset_peer: Option<ChatId>,
        limit: usize,
    ) -> (Vec<ChatId>, usize) {
        let order = self.dialog_order(folder);
        let start = match offset_peer.and_then(|p| order.iter().position(|c| *c == p)) {
            Some(at) => at + 1,
            None if offset_date > 0 && offset_date < i32::MAX => order
                .iter()
                .position(|c| self.top(*c).map_or(0, |m| m.date) < offset_date)
                .unwrap_or(order.len()),
            None => 0,
        };
        let limit = limit.clamp(1, DIALOGS_LIMIT);
        let page = order.iter().skip(start).take(limit).copied().collect();
        (page, order.len())
    }

    /// `messages.getDialogs`: the whole list as `messages.dialogs` when one page holds it, else a
    /// `messages.dialogsSlice` with the list's length.
    pub fn dialogs_tl(
        &self,
        folder: i32,
        offset_date: i32,
        offset_peer: Option<ChatId>,
        limit: i32,
    ) -> tl::enums::messages::Dialogs {
        let limit = usize::try_from(limit).unwrap_or(0);
        let (page, total) = self.dialogs_page(folder, offset_date, offset_peer, limit);
        let whole = page.len() == total;
        let (dialogs, messages, chats, users) = self.dialog_bundle(&page);
        if whole {
            tl::enums::messages::Dialogs::Dialogs(tl::types::messages::Dialogs {
                dialogs,
                messages,
                chats,
                users,
            })
        } else {
            tl::enums::messages::Dialogs::Slice(tl::types::messages::DialogsSlice {
                count: i32::try_from(total).unwrap_or(i32::MAX),
                dialogs,
                messages,
                chats,
                users,
            })
        }
    }

    /// The dialogs of `chats` with their top messages and the chats and users they name.
    fn dialog_bundle(
        &self,
        chats: &[ChatId],
    ) -> (
        Vec<tl::enums::Dialog>,
        Vec<tl::enums::Message>,
        Vec<tl::enums::Chat>,
        Vec<tl::enums::User>,
    ) {
        let dialogs = chats.iter().filter_map(|c| self.dialog_tl(*c)).collect();
        let tops: Vec<(ChatId, &Msg)> = chats
            .iter()
            .filter_map(|c| self.top(*c).map(|m| (*c, m)))
            .collect();
        let messages = tops.iter().map(|(c, m)| self.message_tl(*c, m)).collect();
        let (chats, users) = self.peers_named(chats, tops.iter().map(|(_, m)| *m));
        (dialogs, messages, chats, users)
    }

    /// The chats and users that `chats` and the senders of `msgs` name, in id order.
    pub fn peers_named<'a>(
        &self,
        chats: &[ChatId],
        msgs: impl Iterator<Item = &'a Msg>,
    ) -> (Vec<tl::enums::Chat>, Vec<tl::enums::User>) {
        let mut cs = BTreeSet::new();
        let mut us = BTreeSet::new();
        for c in chats {
            match c {
                ChatId::User(u) => {
                    us.insert(*u);
                }
                _ => {
                    cs.insert(*c);
                    if let Some(to) = self.chats.get(c).and_then(|x| x.migrated_to) {
                        cs.insert(ChatId::Channel(to));
                    }
                    if let Some((from, _)) = self.chats.get(c).and_then(|x| x.migrated_from) {
                        cs.insert(ChatId::Basic(from));
                    }
                }
            }
        }
        for m in msgs {
            if self.users.contains_key(&m.from) {
                us.insert(m.from);
            }
        }
        let chats = cs.into_iter().filter_map(|c| self.chat_tl(c)).collect();
        let users = us.into_iter().filter_map(|u| self.user_tl(u)).collect();
        (chats, users)
    }

    /// The dialog of `chat` (`None` for a chat the world does not hold).
    pub fn dialog_tl(&self, chat: ChatId) -> Option<tl::enums::Dialog> {
        let c = self.chats.get(&chat)?;
        let pts = chat.channel().and_then(|ch| self.channel_pts(ch));
        Some(tl::enums::Dialog::Dialog(tl::types::Dialog {
            pinned: false,
            unread_mark: false,
            view_forum_as_messages: false,
            peer: chat.peer(),
            top_message: self.top(chat).map_or(0, |m| m.id),
            read_inbox_max_id: c.read_inbox_max_id,
            read_outbox_max_id: c.read_outbox_max_id,
            unread_count: c.unread_count(),
            unread_mentions_count: 0,
            unread_reactions_count: 0,
            unread_poll_votes_count: 0,
            notify_settings: tlb::notify_settings(),
            pts,
            draft: None,
            folder_id: c.archived.then_some(1),
            ttl_period: None,
        }))
    }

    /// `messages.getPinnedDialogs`: the world pins nothing (either folder).
    pub fn pinned_dialogs_tl(&self) -> tl::enums::messages::PeerDialogs {
        self.peer_dialogs_of(&[])
    }

    /// `messages.getPeerDialogs`: the dialogs of the named peers the world holds.
    pub fn peer_dialogs_tl(
        &self,
        peers: &[tl::enums::InputDialogPeer],
    ) -> tl::enums::messages::PeerDialogs {
        let chats: Vec<ChatId> = peers
            .iter()
            .filter_map(|p| match p {
                tl::enums::InputDialogPeer::Peer(p) => self.chat_of_input(&p.peer),
                tl::enums::InputDialogPeer::Folder(_)
                | tl::enums::InputDialogPeer::Community(_) => None,
            })
            .collect();
        self.peer_dialogs_of(&chats)
    }

    fn peer_dialogs_of(&self, chats: &[ChatId]) -> tl::enums::messages::PeerDialogs {
        let (dialogs, messages, chats, users) = self.dialog_bundle(chats);
        tl::enums::messages::PeerDialogs::Dialogs(tl::types::messages::PeerDialogs {
            dialogs,
            messages,
            chats,
            users,
            state: self.state(),
        })
    }

    /// The ids `messages.getHistory` answers for `chat`, newest first: older than `offset_id`
    /// (0: from the newest), moved by `add_offset`, at most `limit`, inside `min_id..max_id`.
    pub fn history_ids(&self, chat: ChatId, q: HistoryQuery) -> (Vec<i32>, usize) {
        let Some(c) = self.chats.get(&chat) else {
            return (Vec::new(), 0);
        };
        let desc: Vec<i32> = c.msgs.keys().rev().copied().collect();
        let base = if q.offset_id == 0 {
            0
        } else {
            desc.iter()
                .position(|id| *id < q.offset_id)
                .unwrap_or(desc.len())
        };
        let start = usize::try_from(i64::try_from(base).unwrap_or(0) + i64::from(q.add_offset))
            .unwrap_or(0);
        let limit = usize::try_from(q.limit.clamp(1, 100)).unwrap_or(1);
        let ids = desc
            .iter()
            .skip(start)
            .take(limit)
            .copied()
            .filter(|id| (q.max_id == 0 || *id < q.max_id) && *id > q.min_id)
            .collect();
        (ids, desc.len())
    }

    /// `messages.getHistory`.
    pub fn history_tl(&self, chat: ChatId, q: HistoryQuery) -> tl::enums::messages::Messages {
        let (ids, total) = self.history_ids(chat, q);
        let msgs: Vec<&Msg> = ids
            .iter()
            .filter_map(|id| self.chats.get(&chat)?.msgs.get(id))
            .collect();
        self.messages_answer(chat, &msgs, total)
    }

    fn messages_answer(
        &self,
        chat: ChatId,
        msgs: &[&Msg],
        total: usize,
    ) -> tl::enums::messages::Messages {
        let messages: Vec<tl::enums::Message> =
            msgs.iter().map(|m| self.message_tl(chat, m)).collect();
        let (chats, users) = self.peers_named(&[chat], msgs.iter().copied());
        let count = i32::try_from(total).unwrap_or(i32::MAX);
        if let Some(ch) = chat.channel() {
            tl::enums::messages::Messages::ChannelMessages(tl::types::messages::ChannelMessages {
                inexact: false,
                pts: self.channel_pts(ch).unwrap_or(1),
                count,
                offset_id_offset: None,
                messages,
                topics: Vec::new(),
                chats,
                users,
            })
        } else if messages.len() == total {
            tl::enums::messages::Messages::Messages(tl::types::messages::Messages {
                messages,
                topics: Vec::new(),
                chats,
                users,
            })
        } else {
            tl::enums::messages::Messages::Slice(tl::types::messages::MessagesSlice {
                inexact: false,
                count,
                next_rate: None,
                offset_id_offset: None,
                search_flood: None,
                messages,
                topics: Vec::new(),
                chats,
                users,
            })
        }
    }

    /// `messages.getMessages`: the common box's messages by id (an id the world lacks is left out).
    pub fn messages_by_id_tl(
        &self,
        ids: &[tl::enums::InputMessage],
    ) -> tl::enums::messages::Messages {
        let wanted: BTreeSet<i32> = ids
            .iter()
            .filter_map(|m| match m {
                tl::enums::InputMessage::Id(i) => Some(i.id),
                _ => None,
            })
            .collect();
        let mut messages = Vec::new();
        let mut chats = Vec::new();
        let mut found: Vec<&Msg> = Vec::new();
        for (cid, c) in &self.chats {
            if cid.channel().is_some() {
                continue;
            }
            for id in &wanted {
                if let Some(m) = c.msgs.get(id) {
                    messages.push(self.message_tl(*cid, m));
                    chats.push(*cid);
                    found.push(m);
                }
            }
        }
        let (chats, users) = self.peers_named(&chats, found.into_iter());
        tl::enums::messages::Messages::Messages(tl::types::messages::Messages {
            messages,
            topics: Vec::new(),
            chats,
            users,
        })
    }

    /// `channels.getMessages`.
    pub fn channel_messages_tl(
        &self,
        channel: i64,
        ids: &[tl::enums::InputMessage],
    ) -> Result<tl::enums::messages::Messages, &'static str> {
        let chat = ChatId::Channel(channel);
        let c = self.chats.get(&chat).ok_or("CHANNEL_INVALID")?;
        let msgs: Vec<&Msg> = ids
            .iter()
            .filter_map(|m| match m {
                tl::enums::InputMessage::Id(i) => c.msgs.get(&i.id),
                _ => None,
            })
            .collect();
        let n = msgs.len();
        Ok(self.messages_answer(chat, &msgs, n))
    }

    /// `messages.getChats` / `channels.getChannels`: the groups and channels the world holds.
    pub fn chats_tl(&self, ids: impl Iterator<Item = ChatId>) -> tl::enums::messages::Chats {
        tl::enums::messages::Chats::Chats(tl::types::messages::Chats {
            chats: ids.filter_map(|c| self.chat_tl(c)).collect(),
        })
    }

    /// `channels.getFullChannel`.
    pub fn full_channel_tl(
        &self,
        channel: i64,
    ) -> Result<tl::enums::messages::ChatFull, &'static str> {
        let chat = ChatId::Channel(channel);
        let c = self.chats.get(&chat).ok_or("CHANNEL_INVALID")?;
        let full = tlb::channel_full(tlb::ChannelFullParts {
            id: channel,
            read_inbox_max_id: c.read_inbox_max_id,
            read_outbox_max_id: c.read_outbox_max_id,
            unread_count: c.unread_count(),
            pts: self.channel_pts(channel).unwrap_or(1),
            migrated_from: c.migrated_from,
        });
        let (chats, users) = self.peers_named(&[chat], std::iter::empty());
        Ok(tl::enums::messages::ChatFull::Full(
            tl::types::messages::ChatFull {
                full_chat: full,
                chats,
                users,
            },
        ))
    }

    /// The users of a basic group: the account and the first two peers.
    fn members(&self) -> Vec<i64> {
        self.users.keys().copied().take(3).collect()
    }

    /// `messages.getFullChat`.
    pub fn full_chat_tl(
        &self,
        chat_id: i64,
    ) -> Result<tl::enums::messages::ChatFull, &'static str> {
        let chat = ChatId::Basic(chat_id);
        if !self.chats.contains_key(&chat) {
            return Err("CHAT_ID_INVALID");
        }
        let members = self.members();
        let full = tlb::chat_full(chat_id, &members, self.date);
        let (chats, _) = self.peers_named(&[chat], std::iter::empty());
        let users = members.iter().filter_map(|u| self.user_tl(*u)).collect();
        Ok(tl::enums::messages::ChatFull::Full(
            tl::types::messages::ChatFull {
                full_chat: full,
                chats,
                users,
            },
        ))
    }

    /// `updates.channelDifferenceTooLong` for `channel`: its dialog and its latest messages.
    pub fn channel_too_long_tl(
        &self,
        channel: i64,
    ) -> Option<tl::enums::updates::ChannelDifference> {
        let chat = ChatId::Channel(channel);
        let c = self.chats.get(&chat)?;
        let latest: Vec<&Msg> = c.msgs.values().rev().take(TOO_LONG_MESSAGES).collect();
        let dialog = self.dialog_tl(chat)?;
        let messages = latest.iter().map(|m| self.message_tl(chat, m)).collect();
        let (chats, users) = self.peers_named(&[chat], latest.iter().copied());
        Some(tl::enums::updates::ChannelDifference::TooLong(
            tl::types::updates::ChannelDifferenceTooLong {
                r#final: true,
                timeout: None,
                dialog,
                messages,
                chats,
                users,
            },
        ))
    }

    /// A peer calls the account: the call's id (its `phoneCallRequested` is the push's).
    pub fn call_requested(&mut self, caller: i64) -> i64 {
        let id = FIRST_CALL_ID + i64::try_from(self.calls.len()).unwrap_or(0);
        self.calls.insert(id, caller);
        id
    }

    /// The call `call_id` ended unanswered: the call line in the caller's private chat.
    pub fn call_missed(&mut self, call_id: i64) -> Option<Logged> {
        let caller = *self.calls.get(&call_id)?;
        Some(self.new_service(
            ChatId::User(caller),
            caller,
            Service::MissedCall { call_id },
        ))
    }

    /// The basic group `chat_id` becomes the supergroup `channel_id`: the group's last line
    /// (`messageActionChatMigrateTo`) and the supergroup's first (`messageActionChannelMigrateFrom`).
    pub fn upgrade(&mut self, chat_id: i64, channel_id: i64) -> Option<(Logged, Logged)> {
        let basic = ChatId::Basic(chat_id);
        if self.chats.get(&basic)?.migrated_to.is_some() {
            return None;
        }
        let title = match &self.chats.get(&basic)?.kind {
            Kind::Basic { title } => title.clone(),
            _ => return None,
        };
        let new = ChatId::Channel(channel_id);
        if self.chats.contains_key(&new) {
            return None;
        }
        let first = self.new_service(basic, OWN_USER_ID, Service::ChatMigrateTo { channel_id });
        let last_id = match &first.change {
            Change::New(_, m) => m.id,
            _ => 0,
        };
        let archived = self.chats.get(&basic).is_some_and(|c| c.archived);
        self.add_chat(
            new,
            Kind::Super {
                title: title.clone(),
                username: None,
            },
            archived,
        );
        if let Some(c) = self.chats.get_mut(&basic) {
            c.migrated_to = Some(channel_id);
        }
        if let Some(c) = self.chats.get_mut(&new) {
            c.migrated_from = Some((chat_id, last_id));
        }
        let second = self.new_service(
            new,
            OWN_USER_ID,
            Service::ChannelMigrateFrom { title, chat_id },
        );
        Some((first, second))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::{Deserializable, Serializable};

    #[test]
    fn the_dialog_pages_cover_every_chat_once_and_end_after_the_last_page() {
        let w = World::standard(10_000);
        for limit in 1..=8 {
            let mut seen = Vec::new();
            let (mut offset_date, mut offset_peer) = (0, None);
            for _ in 0..20 {
                let (page, total) = w.dialogs_page(0, offset_date, offset_peer, limit);
                assert_eq!(
                    total, 6,
                    "the main list: two private chats, the group, three channels"
                );
                let Some(last) = page.last().copied() else {
                    break;
                };
                seen.extend(page);
                offset_date = w.top(last).unwrap().date;
                offset_peer = Some(last);
            }
            assert_eq!(seen, w.dialog_order(0), "limit {limit}");
        }
        assert_eq!(w.dialog_order(1), vec![ChatId::User(203)], "the archive");
        // The newest first: the broadcast channel's post is the world's last message.
        assert_eq!(w.dialog_order(0)[0], ChatId::Channel(403));
        // Paging by the date alone (an offset peer the world does not know).
        let (page, _) = w.dialogs_page(0, w.top(ChatId::Channel(403)).unwrap().date, None, 100);
        assert_eq!(page, w.dialog_order(0)[1..]);
    }

    #[test]
    fn one_page_is_the_whole_list_and_a_smaller_one_a_slice_with_the_count() {
        let w = World::standard(10_000);
        let whole = w.dialogs_tl(0, 0, None, 100).to_bytes();
        let tl::enums::messages::Dialogs::Dialogs(d) =
            tl::enums::messages::Dialogs::from_bytes(&whole).unwrap()
        else {
            panic!("messages.dialogs");
        };
        assert_eq!((d.dialogs.len(), d.messages.len()), (6, 6));
        assert!(d.users.len() >= 2 && d.chats.len() == 4);
        let tl::enums::messages::Dialogs::Slice(s) = w.dialogs_tl(0, 0, None, 2) else {
            panic!("a slice");
        };
        assert_eq!((s.count, s.dialogs.len()), (6, 2));
        let tl::enums::Dialog::Dialog(a) = w.dialog_tl(ChatId::User(201)).unwrap() else {
            panic!("a dialog");
        };
        assert_eq!((a.unread_count, a.folder_id, a.pts), (3, None, None));
        let tl::enums::Dialog::Dialog(c) = w.dialog_tl(ChatId::Channel(401)).unwrap() else {
            panic!("a dialog");
        };
        assert_eq!((c.unread_count, c.pts), (1, w.channel_pts(401)));
        let tl::enums::Dialog::Dialog(r) = w.dialog_tl(ChatId::User(203)).unwrap() else {
            panic!("a dialog");
        };
        assert_eq!(r.folder_id, Some(1));
    }

    #[test]
    fn the_history_pages_from_the_newest_by_offset_id_and_add_offset() {
        let w = World::standard(10_000);
        let a = ChatId::User(201);
        let q = |offset_id, add_offset, limit| HistoryQuery {
            offset_id,
            add_offset,
            limit,
            ..HistoryQuery::default()
        };
        assert_eq!(w.history_ids(a, q(0, 0, 20)), (vec![4, 3, 2, 1], 4));
        assert_eq!(w.history_ids(a, q(0, 0, 1)).0, vec![4]);
        assert_eq!(w.history_ids(a, q(4, 0, 20)).0, vec![3, 2, 1]);
        assert_eq!(w.history_ids(a, q(3, -1, 2)).0, vec![3, 2]);
        assert_eq!(w.history_ids(a, q(1, 0, 20)).0, Vec::<i32>::new());
        let tl::enums::messages::Messages::Slice(s) = w.history_tl(a, q(0, 0, 2)) else {
            panic!("a slice");
        };
        assert_eq!((s.count, s.messages.len()), (4, 2));
        let tl::enums::messages::Messages::ChannelMessages(c) =
            w.history_tl(ChatId::Channel(401), q(0, 0, 20))
        else {
            panic!("channel messages");
        };
        assert_eq!((c.count, c.pts), (1, w.channel_pts(401).unwrap()));
    }

    #[test]
    fn messages_chats_and_full_chats_come_from_the_world() {
        let w = World::standard(10_000);
        let id = |id| tl::enums::InputMessage::Id(tl::types::InputMessageId { id });
        let tl::enums::messages::Messages::Messages(m) = w.messages_by_id_tl(&[id(2), id(999)])
        else {
            panic!("messages");
        };
        assert_eq!(m.messages.len(), 1);
        assert!(w.channel_messages_tl(401, &[id(1)]).is_ok());
        assert_eq!(
            w.channel_messages_tl(499, &[id(1)]).err(),
            Some("CHANNEL_INVALID")
        );
        let tl::enums::messages::Chats::Chats(c) =
            w.chats_tl([ChatId::Basic(301), ChatId::Channel(401), ChatId::Basic(9)].into_iter())
        else {
            panic!("messages.chats");
        };
        assert_eq!(c.chats.len(), 2);
        let full = w.full_channel_tl(401).unwrap().to_bytes();
        let tl::enums::messages::ChatFull::Full(f) =
            tl::enums::messages::ChatFull::from_bytes(&full).unwrap();
        assert!(matches!(f.full_chat, tl::enums::ChatFull::ChannelFull(c) if c.id == 401));
        let full = w.full_chat_tl(301).unwrap().to_bytes();
        let tl::enums::messages::ChatFull::Full(f) =
            tl::enums::messages::ChatFull::from_bytes(&full).unwrap();
        assert_eq!(f.users.len(), 3);
        assert!(w.full_chat_tl(302).is_err());
    }

    #[test]
    fn a_too_long_channel_difference_carries_the_dialog_and_the_latest_messages() {
        let mut w = World::standard(10_000);
        for n in 0..5 {
            w.new_message(ChatId::Channel(401), 202, &format!("newer {n}"));
        }
        w.set_channel_too_long_after(401, 2);
        let bytes = w.channel_difference_tl(401, 1, 100).unwrap().to_bytes();
        let tl::enums::updates::ChannelDifference::TooLong(t) =
            tl::enums::updates::ChannelDifference::from_bytes(&bytes).unwrap()
        else {
            panic!("too long");
        };
        let tl::enums::Dialog::Dialog(d) = t.dialog else {
            panic!("a dialog");
        };
        assert_eq!((d.pts, d.top_message), (w.channel_pts(401), 6));
        assert_eq!(t.messages.len(), 6);
    }

    #[test]
    fn world_json_names_every_original_and_escapes_it() {
        let mut w = World::standard(10_000);
        w.new_message(ChatId::User(201), 201, "a \"quoted\"\nline");
        let j = w.to_json();
        for s in [
            "Peer Alpha",
            "999060000201",
            "Group Delta",
            "publicfoxtrot",
            "alpha two",
            "\"c403\"",
            "a \\\"quoted\\\"\\nline",
        ] {
            assert!(j.contains(s), "{s} missing");
        }
        assert_eq!(ChatId::parse("g301"), Some(ChatId::Basic(301)));
        assert_eq!(ChatId::parse("x1"), None);
        assert_eq!(ChatId::parse("u0"), None);
    }
}
