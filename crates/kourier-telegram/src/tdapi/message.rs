//! TG-3 C1: the message objects of td_api (the pinned 1.8.69) that T3 reads, and the message
//! updates (brief §2.2). A message's `content` stays RAW ([`Raw`]) until the content map (C4a,
//! C4b) reads it; its `Debug` prints the content's `@type` only. Free text is a [`Text`].

use serde::Deserialize;

use super::{Raw, Text, int64};

/// `MessageSender` (`td_api.tl:3141-3144`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum MessageSender {
    #[serde(rename = "messageSenderUser")]
    User {
        #[serde(default)]
        user_id: i64,
    },
    /// A chat (a channel's post, an anonymous admin): Kourier's author `tg:<negative id>` (L-8).
    #[serde(rename = "messageSenderChat")]
    Chat {
        #[serde(default)]
        chat_id: i64,
    },
    #[default]
    #[serde(other)]
    Unknown,
}

/// `MessageSendingState` (`td_api.tl:3344-3354`): a message being sent (or failed) from THIS
/// client: never a received row (L5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum SendingState {
    #[serde(rename = "messageSendingStatePending")]
    Pending {
        #[serde(default)]
        sending_id: i32,
    },
    #[serde(rename = "messageSendingStateFailed")]
    Failed,
    #[serde(other)]
    Unknown,
}

/// `MessageSchedulingState` (`td_api.tl:6236-6243`): a scheduled message: never a row (L5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum SchedulingState {
    #[serde(rename = "messageSchedulingStateSendAtDate")]
    SendAtDate {
        #[serde(default)]
        send_date: i32,
    },
    #[serde(rename = "messageSchedulingStateSendWhenOnline")]
    SendWhenOnline,
    #[serde(rename = "messageSchedulingStateSendWhenVideoProcessed")]
    SendWhenVideoProcessed {
        #[serde(default)]
        send_date: i32,
    },
    #[serde(other)]
    Unknown,
}

/// `MessageSelfDestructType` (`td_api.tl:6249-6252`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum SelfDestructType {
    #[serde(rename = "messageSelfDestructTypeTimer")]
    Timer {
        #[serde(default)]
        self_destruct_time: i32,
    },
    /// View once.
    #[serde(rename = "messageSelfDestructTypeImmediately")]
    Immediately,
    #[serde(other)]
    Unknown,
}

/// `MessageReplyTo` (`td_api.tl:3383-3386`): the ids only (the quote and the origin are T5's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum ReplyTo {
    #[serde(rename = "messageReplyToMessage")]
    Message {
        #[serde(default)]
        chat_id: i64,
        #[serde(default)]
        message_id: i64,
    },
    #[serde(rename = "messageReplyToStory")]
    Story {
        #[serde(default)]
        story_poster_chat_id: i64,
        #[serde(default)]
        story_id: i32,
    },
    #[serde(other)]
    Unknown,
}

/// `MessageTopic` (`td_api.tl:3311-3320`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum MessageTopic {
    #[serde(rename = "messageTopicThread")]
    Thread {
        #[serde(default)]
        message_thread_id: i64,
    },
    #[serde(rename = "messageTopicForum")]
    Forum {
        #[serde(default)]
        forum_topic_id: i32,
    },
    #[serde(rename = "messageTopicDirectMessages")]
    DirectMessages {
        #[serde(default)]
        direct_messages_chat_topic_id: i64,
    },
    #[serde(rename = "messageTopicSavedMessages")]
    SavedMessages {
        #[serde(default)]
        saved_messages_topic_id: i64,
    },
    #[serde(other)]
    Unknown,
}

/// `message` (`td_api.tl:3475`): what T3 reads. Ids are full ids (a server id is `id >> 20`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct Message {
    #[serde(default)]
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) sender_id: MessageSender,
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) sending_state: Option<SendingState>,
    #[serde(default)]
    pub(crate) scheduling_state: Option<SchedulingState>,
    #[serde(default)]
    pub(crate) is_outgoing: bool,
    #[serde(default)]
    pub(crate) is_channel_post: bool,
    /// False: protected content (no forward, no save).
    #[serde(default)]
    pub(crate) can_be_saved: bool,
    #[serde(default)]
    pub(crate) contains_unread_mention: bool,
    #[serde(default)]
    pub(crate) date: i32,
    #[serde(default)]
    pub(crate) edit_date: i32,
    /// `messageForwardInfo` (`td_api.tl:3264`), raw until the content map reads it.
    #[serde(default)]
    pub(crate) forward_info: Option<Raw>,
    #[serde(default)]
    pub(crate) reply_to: Option<ReplyTo>,
    #[serde(default)]
    pub(crate) topic_id: Option<MessageTopic>,
    #[serde(default)]
    pub(crate) self_destruct_type: Option<SelfDestructType>,
    /// Seconds left before the self-destruct timer ends (a td_api double, rounded up); 0 = it has not
    /// started (the message was not opened yet). TG-3 C4a: `content::message` turns it into `Expiry`.
    #[serde(default, deserialize_with = "super::secs")]
    pub(crate) self_destruct_in: i64,
    /// Seconds left before the chat's auto-delete timer deletes the message (a td_api double, rounded
    /// up); 0 = never.
    #[serde(default, deserialize_with = "super::secs")]
    pub(crate) auto_delete_in: i64,
    /// An int64: a JSON string; 0 = no album.
    #[serde(default, deserialize_with = "int64")]
    pub(crate) media_album_id: i64,
    #[serde(default)]
    pub(crate) restriction_info: Option<super::RestrictionInfo>,
    /// The `MessageContent`, raw (C4a/C4b map it).
    #[serde(default)]
    pub(crate) content: Raw,
}

/// `TextEntityType` (`td_api.tl:6050-6119`). Offsets and lengths are UTF-16 code units.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum TextEntityType {
    #[serde(rename = "textEntityTypeMention")]
    Mention,
    #[serde(rename = "textEntityTypeHashtag")]
    Hashtag,
    #[serde(rename = "textEntityTypeCashtag")]
    Cashtag,
    #[serde(rename = "textEntityTypeBotCommand")]
    BotCommand,
    #[serde(rename = "textEntityTypeUrl")]
    Url,
    #[serde(rename = "textEntityTypeEmailAddress")]
    EmailAddress,
    #[serde(rename = "textEntityTypePhoneNumber")]
    PhoneNumber,
    #[serde(rename = "textEntityTypeBankCardNumber")]
    BankCardNumber,
    #[serde(rename = "textEntityTypeTonAddress")]
    TonAddress,
    #[serde(rename = "textEntityTypeBold")]
    Bold,
    #[serde(rename = "textEntityTypeItalic")]
    Italic,
    #[serde(rename = "textEntityTypeUnderline")]
    Underline,
    #[serde(rename = "textEntityTypeStrikethrough")]
    Strikethrough,
    #[serde(rename = "textEntityTypeSpoiler")]
    Spoiler,
    #[serde(rename = "textEntityTypeCode")]
    Code,
    #[serde(rename = "textEntityTypePre")]
    Pre,
    #[serde(rename = "textEntityTypePreCode")]
    PreCode {
        #[serde(default)]
        language: Text,
    },
    #[serde(rename = "textEntityTypeBlockQuote")]
    BlockQuote,
    #[serde(rename = "textEntityTypeExpandableBlockQuote")]
    ExpandableBlockQuote,
    /// A hidden link: `Link(url)` for http(s), the UI shows the target before opening (L-10, `markup`).
    #[serde(rename = "textEntityTypeTextUrl")]
    TextUrl {
        #[serde(default)]
        url: Text,
    },
    #[serde(rename = "textEntityTypeMentionName")]
    MentionName {
        #[serde(default)]
        user_id: i64,
    },
    #[serde(rename = "textEntityTypeCustomEmoji")]
    CustomEmoji {
        #[serde(default, deserialize_with = "int64")]
        custom_emoji_id: i64,
    },
    #[serde(rename = "textEntityTypeMediaTimestamp")]
    MediaTimestamp {
        #[serde(default)]
        media_timestamp: i32,
    },
    #[serde(rename = "textEntityTypeDateTime")]
    DateTime {
        #[serde(default)]
        unix_time: i32,
    },
    #[default]
    #[serde(other)]
    Unknown,
}

/// `textEntity` (`td_api.tl:110`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct TextEntity {
    #[serde(default)]
    pub(crate) offset: i32,
    #[serde(default)]
    pub(crate) length: i32,
    #[serde(default, rename = "type")]
    pub(crate) r#type: TextEntityType,
}

/// `formattedText` (`td_api.tl:117`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct FormattedText {
    #[serde(default)]
    pub(crate) text: Text,
    #[serde(default)]
    pub(crate) entities: Vec<TextEntity>,
}

/// `ChatAction` (`td_api.tl:6694-6736`): typing and its kin (ephemeral).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum ChatAction {
    #[serde(rename = "chatActionTyping")]
    Typing,
    #[serde(rename = "chatActionRecordingVideo")]
    RecordingVideo,
    #[serde(rename = "chatActionUploadingVideo")]
    UploadingVideo,
    #[serde(rename = "chatActionRecordingVoiceNote")]
    RecordingVoiceNote,
    #[serde(rename = "chatActionUploadingVoiceNote")]
    UploadingVoiceNote,
    #[serde(rename = "chatActionUploadingPhoto")]
    UploadingPhoto,
    #[serde(rename = "chatActionUploadingDocument")]
    UploadingDocument,
    #[serde(rename = "chatActionChoosingSticker")]
    ChoosingSticker,
    #[serde(rename = "chatActionChoosingLocation")]
    ChoosingLocation,
    #[serde(rename = "chatActionChoosingContact")]
    ChoosingContact,
    #[serde(rename = "chatActionStartPlayingGame")]
    StartPlayingGame,
    #[serde(rename = "chatActionRecordingVideoNote")]
    RecordingVideoNote,
    #[serde(rename = "chatActionUploadingVideoNote")]
    UploadingVideoNote,
    /// Its emoji is not kept.
    #[serde(rename = "chatActionWatchingAnimations")]
    WatchingAnimations,
    #[serde(rename = "chatActionCancel")]
    Cancel,
    #[default]
    #[serde(other)]
    Unknown,
}

// The message updates (`td_api.tl:10763-11070`).

/// `updateNewMessage` (`td_api.tl:10763`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct NewMessage {
    #[serde(default)]
    pub(crate) message: Message,
}

/// `updateMessageContent` (`td_api.tl:10784`): alone it is no edit (M1).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct MessageContent {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) message_id: i64,
    #[serde(default)]
    pub(crate) new_content: Raw,
}

/// `updateMessageEdited` (`td_api.tl:10794`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct MessageEdited {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) message_id: i64,
    #[serde(default)]
    pub(crate) edit_date: i32,
}

/// `updateDeleteMessages` (`td_api.tl:11063`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct DeleteMessages {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) message_ids: Vec<i64>,
    #[serde(default)]
    pub(crate) is_permanent: bool,
    #[serde(default)]
    pub(crate) from_cache: bool,
}

/// `updateChatAction` (`td_api.tl:11070`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatActionUpdate {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) topic_id: Option<MessageTopic>,
    #[serde(default)]
    pub(crate) sender_id: MessageSender,
    #[serde(default)]
    pub(crate) action: ChatAction,
}
