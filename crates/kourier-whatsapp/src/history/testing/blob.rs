//! Hand-made `HistorySync` chunks, as the phone uploads them: protobuf, then zlib.
//!
//! Shared by the library's unit tests (`history::testing`) and the integration tests
//! (`tests/support/history.rs` includes this very file), so it names only what both can reach:
//! `waproto` and `flate2`. Every id, number and name a test puts in is a synthetic value of its own.
//!
//! ```text
//! Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)        // sync type, chunk order, progress
//!     .chat(Chat::new("<jid>").unread(2).message(Msg::text("<jid>", "<id>", 1_700_000_100, "hi")))
//!     .push_name("<jid>", "A Name")
//!     .blob()                                           // the bytes the capture hook is given
//! ```

#![allow(dead_code)]

use std::io::Write as _;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use waproto::buffa::{Message as _, MessageField};
use waproto::whatsapp as wa;

/// The sync type of a chunk (`HistorySync.syncType`).
pub(crate) use wa::history_sync::HistorySyncType as SyncType;
/// The delivery state of a message of ours (`WebMessageInfo.status`).
pub(crate) use wa::web_message_info::Status;

/// One chunk: what the phone uploads as one `HistorySync`.
#[derive(Clone, Debug)]
pub(crate) struct Chunk {
    pub(crate) sync: wa::HistorySync,
}

impl Chunk {
    /// A chunk with no chats: its sync type, its `chunkOrder` and its `progress` (0 to 100).
    pub(crate) fn new(sync_type: SyncType, order: u32, progress: u32) -> Self {
        let mut sync = wa::HistorySync::default();
        sync.sync_type = sync_type;
        sync.chunk_order = Some(order);
        sync.progress = Some(progress);
        Self { sync }
    }

    /// Adds a chat, in the order given (the phone sends them in its own order).
    pub(crate) fn chat(mut self, chat: Chat) -> Self {
        self.sync.conversations.push(chat.conversation);
        self
    }

    /// Adds a push name (`HistorySync.pushnames`): the name the person with `jid` calls themselves.
    pub(crate) fn push_name(mut self, jid: &str, name: &str) -> Self {
        let mut entry = wa::Pushname::default();
        entry.id = Some(jid.to_owned());
        entry.pushname = Some(name.to_owned());
        self.sync.pushnames.push(entry);
        self
    }

    /// Adds a pair of ids of one person (`HistorySync.phoneNumberToLidMappings`).
    pub(crate) fn pair(mut self, pn_jid: &str, lid_jid: &str) -> Self {
        let mut entry = wa::PhoneNumberToLIDMapping::default();
        entry.pn_jid = Some(pn_jid.to_owned());
        entry.lid_jid = Some(lid_jid.to_owned());
        self.sync.phone_number_to_lid_mappings.push(entry);
        self
    }

    /// The protobuf bytes, not compressed.
    pub(crate) fn proto(&self) -> Vec<u8> {
        self.sync.encode_to_vec()
    }

    /// The blob the capture hook is given: the protobuf as a zlib stream, as the phone uploads it.
    pub(crate) fn blob(&self) -> Vec<u8> {
        zlib(&self.proto())
    }
}

/// One chat of a chunk (`Conversation`).
#[derive(Clone, Debug)]
pub(crate) struct Chat {
    pub(crate) conversation: wa::Conversation,
}

impl Chat {
    /// A chat with the given `id` (a user, group or LID jid) and nothing else.
    pub(crate) fn new(id: &str) -> Self {
        let mut conversation = wa::Conversation::default();
        conversation.id = id.to_owned();
        Self { conversation }
    }

    /// `lidJid`: the chat's LID form (the phone states it on a chat it knows both forms of).
    pub(crate) fn lid(mut self, jid: &str) -> Self {
        self.conversation.lid_jid = Some(jid.to_owned());
        self
    }

    /// `pnJid`: the chat's phone-number form.
    pub(crate) fn pn(mut self, jid: &str) -> Self {
        self.conversation.pn_jid = Some(jid.to_owned());
        self
    }

    /// `name`: a group's title.
    pub(crate) fn name(mut self, name: &str) -> Self {
        self.conversation.name = Some(name.to_owned());
        self
    }

    /// `tcToken`, `tcTokenTimestamp` (s) and `tcTokenSenderTimestamp`: the chat's trusted-contact token.
    pub(crate) fn token(mut self, bytes: &[u8], at: u64, sender_at: Option<u64>) -> Self {
        self.conversation.tc_token = Some(bytes.to_vec());
        self.conversation.tc_token_timestamp = Some(at);
        self.conversation.tc_token_sender_timestamp = sender_at;
        self
    }

    /// `unreadCount`: how many messages the phone counts as unread (WhatsApp states a count only).
    pub(crate) fn unread(mut self, count: u32) -> Self {
        self.conversation.unread_count = Some(count);
        self
    }

    /// `ephemeralExpiration`: the chat's disappearing timer in seconds.
    pub(crate) fn timer(mut self, seconds: u32) -> Self {
        self.conversation.ephemeral_expiration = Some(seconds);
        self
    }

    /// `conversationTimestamp`: when the chat was last active, in seconds.
    pub(crate) fn active_at(mut self, seconds: u64) -> Self {
        self.conversation.conversation_timestamp = Some(seconds);
        self
    }

    /// `locked`: a chat behind the phone's Chat Lock.
    pub(crate) fn locked(mut self) -> Self {
        self.conversation.locked = Some(true);
        self
    }

    /// `endOfHistoryTransfer`: the phone's mark that this chat has nothing more to send.
    pub(crate) fn end_of_history(mut self) -> Self {
        self.conversation.end_of_history_transfer = Some(true);
        self
    }

    /// Adds a message, in the order given (the phone sends the newest first).
    pub(crate) fn message(mut self, message: Msg) -> Self {
        let mut entry = wa::HistorySyncMsg::default();
        entry.message = MessageField::some(message.info);
        self.conversation.messages.push(entry);
        self
    }

    /// Adds `count` plain texts of the other side, one second apart from `first_at` (seconds), ids
    /// `<prefix>0`, `<prefix>1` ...: for a chat of a few hundred messages that only has to be long.
    pub(crate) fn texts(mut self, prefix: &str, first_at: u64, count: usize) -> Self {
        for n in 0..count {
            let id = format!("{prefix}{n}");
            let body = format!("message {n} of the chat");
            let message = Msg::text(&self.conversation.id, &id, first_at + n as u64, &body);
            self = self.message(message);
        }
        self
    }
}

/// One message of a chat (`WebMessageInfo`).
#[derive(Clone, Debug)]
pub(crate) struct Msg {
    pub(crate) info: wa::WebMessageInfo,
}

impl Msg {
    /// A text from the other side of the chat `chat`: `fromMe` false, the key's `remoteJid` the chat.
    pub(crate) fn text(chat: &str, id: &str, at: u64, body: &str) -> Self {
        let mut key = wa::MessageKey::default();
        key.remote_jid = Some(chat.to_owned());
        key.from_me = Some(false);
        key.id = Some(id.to_owned());
        let mut message = wa::Message::default();
        message.conversation = Some(body.to_owned());
        let mut info = wa::WebMessageInfo::default();
        info.key = MessageField::some(key);
        info.message = MessageField::some(message);
        info.message_timestamp = Some(at);
        Self { info }
    }

    /// A text of ours (typed on the phone): `fromMe` true, and a delivery state.
    pub(crate) fn mine(chat: &str, id: &str, at: u64, body: &str, status: Status) -> Self {
        let mut message = Self::text(chat, id, at, body);
        if let Some(key) = message.info.key.as_option_mut() {
            key.from_me = Some(true);
        }
        message.info.status = Some(status);
        message
    }

    /// `participant`: the sender of a message in a group, on the key and on the message.
    pub(crate) fn from(mut self, participant_jid: &str) -> Self {
        if let Some(key) = self.info.key.as_option_mut() {
            key.participant = Some(participant_jid.to_owned());
        }
        self.info.participant = Some(participant_jid.to_owned());
        self
    }

    /// `ephemeralDuration` and `ephemeralStartTimestamp`: the message's own disappearing timer.
    pub(crate) fn timer(mut self, seconds: u32, started_at: Option<u64>) -> Self {
        self.info.ephemeral_duration = Some(seconds);
        self.info.ephemeral_start_timestamp = started_at;
        self
    }

    /// `messageSecret`: the 32 bytes that authenticate the message's later edits and reactions.
    pub(crate) fn secret(mut self, secret: &[u8]) -> Self {
        self.info.message_secret = Some(secret.to_vec());
        self
    }

    /// A reaction on the message. `by` is the reacting person's jid, `None` for ours (`fromMe`).
    pub(crate) fn reaction(mut self, by: Option<&str>, emoji: &str, at_ms: i64) -> Self {
        let mut key = wa::MessageKey::default();
        if let Some(chat) = self.info.key.as_option() {
            key.remote_jid.clone_from(&chat.remote_jid);
        }
        key.from_me = Some(by.is_none());
        key.participant = by.map(str::to_owned);
        let mut reaction = wa::Reaction::default();
        reaction.key = MessageField::some(key);
        reaction.text = Some(emoji.to_owned());
        reaction.sender_timestamp_ms = Some(at_ms);
        self.info.reactions.push(reaction);
        self
    }
}

/// `data` as a zlib stream, really compressed (a repetitive chat shrinks to a few percent).
pub(crate) fn zlib(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).expect("compress into memory");
    encoder.finish().expect("finish the zlib stream")
}
