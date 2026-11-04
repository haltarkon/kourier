//! TG-3 C1: the chat, user and group objects of td_api (the pinned 1.8.69) that T3 reads, and the
//! chat updates that carry them (brief §2.2). Every field has `#[serde(default)]`; every tagged
//! enum has an `Unknown` for a type this build does not know. Free text (a title, a name, a
//! number, a username, a reason) is a [`Text`], whose `Debug` prints nothing of it, so the derived
//! `Debug`s here print ids, kinds and counts only.

use serde::Deserialize;

use super::{Text, int64};

/// `ChatList` (`td_api.tl:3828-3834`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum ChatList {
    #[serde(rename = "chatListMain")]
    Main,
    /// Telegram's archive: Kourier's archived chats.
    #[serde(rename = "chatListArchive")]
    Archive,
    /// A folder (T8).
    #[serde(rename = "chatListFolder")]
    Folder {
        #[serde(default)]
        chat_folder_id: i32,
    },
    #[default]
    #[serde(other)]
    Unknown,
}

/// `ChatType` (`td_api.tl:3749-3758`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum ChatType {
    #[serde(rename = "chatTypePrivate")]
    Private {
        #[serde(default)]
        user_id: i64,
    },
    #[serde(rename = "chatTypeBasicGroup")]
    BasicGroup {
        #[serde(default)]
        basic_group_id: i64,
    },
    /// A supergroup or, with `is_channel`, a channel (broadcast: see [`Supergroup`]).
    #[serde(rename = "chatTypeSupergroup")]
    Supergroup {
        #[serde(default)]
        supergroup_id: i64,
        #[serde(default)]
        is_channel: bool,
    },
    /// A secret chat (never on another device: Kourier shows none).
    #[serde(rename = "chatTypeSecret")]
    Secret {
        #[serde(default)]
        secret_chat_id: i32,
        #[serde(default)]
        user_id: i64,
    },
    #[default]
    #[serde(other)]
    Unknown,
}

/// `chatPosition` (`td_api.tl:3854`): `order` 0 = not in that list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatPosition {
    #[serde(default)]
    pub(crate) list: ChatList,
    /// An int64: a JSON string.
    #[serde(default, deserialize_with = "int64")]
    pub(crate) order: i64,
    #[serde(default)]
    pub(crate) is_pinned: bool,
}

/// `chatNotificationSettings` (`td_api.tl:3673`): the mute part.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatNotificationSettings {
    #[serde(default)]
    pub(crate) use_default_mute_for: bool,
    /// Seconds left before unmuted.
    #[serde(default)]
    pub(crate) mute_for: i32,
}

/// `scopeNotificationSettings` (`td_api.tl:3685`): the mute part.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ScopeNotificationSettings {
    #[serde(default)]
    pub(crate) mute_for: i32,
}

/// `NotificationSettingsScope` (`td_api.tl:3647-3653`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum NotificationScope {
    #[serde(rename = "notificationSettingsScopePrivateChats")]
    Private,
    #[serde(rename = "notificationSettingsScopeGroupChats")]
    Group,
    #[serde(rename = "notificationSettingsScopeChannelChats")]
    Channel,
    #[default]
    #[serde(other)]
    Unknown,
}

/// `remoteFile` (`td_api.tl:303`): the ids T6 downloads by.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct RemoteFile {
    #[serde(default)]
    pub(crate) id: Text,
    #[serde(default)]
    pub(crate) unique_id: Text,
}

/// `file` (`td_api.tl:311`): its local id and its remote ids (T3 downloads nothing).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct File {
    #[serde(default)]
    pub(crate) id: i32,
    /// Bytes; 0 when unknown (then `expected_size` is the estimate). TG-3 C4a.
    #[serde(default)]
    pub(crate) size: i64,
    #[serde(default)]
    pub(crate) expected_size: i64,
    #[serde(default)]
    pub(crate) remote: RemoteFile,
}

/// `chatPhotoInfo` (`td_api.tl:763`) and `profilePhoto` (`:755`, its `id` an int64): kept as
/// remote ids for T6.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct PhotoInfo {
    #[serde(default, deserialize_with = "int64")]
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) small: File,
    #[serde(default)]
    pub(crate) big: File,
}

/// `chat` (`td_api.tl:3937`): what T3 reads. `last_message` is null when TDLib has none (or it
/// became unknown).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct Chat {
    #[serde(default)]
    pub(crate) id: i64,
    #[serde(default, rename = "type")]
    pub(crate) r#type: ChatType,
    #[serde(default)]
    pub(crate) title: Text,
    #[serde(default)]
    pub(crate) photo: Option<PhotoInfo>,
    #[serde(default)]
    pub(crate) last_message: Option<Box<super::Message>>,
    #[serde(default)]
    pub(crate) positions: Vec<ChatPosition>,
    #[serde(default)]
    pub(crate) has_protected_content: bool,
    #[serde(default)]
    pub(crate) is_marked_as_unread: bool,
    #[serde(default)]
    pub(crate) unread_count: i32,
    #[serde(default)]
    pub(crate) last_read_inbox_message_id: i64,
    #[serde(default)]
    pub(crate) last_read_outbox_message_id: i64,
    #[serde(default)]
    pub(crate) unread_mention_count: i32,
    #[serde(default)]
    pub(crate) notification_settings: ChatNotificationSettings,
    #[serde(default)]
    pub(crate) message_auto_delete_time: i32,
}

/// `usernames` (`td_api.tl:2681`): a public chat or user has an active one.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct Usernames {
    #[serde(default)]
    pub(crate) active_usernames: Vec<Text>,
}

/// `restrictionInfo` (`td_api.tl:2644`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct RestrictionInfo {
    #[serde(default)]
    pub(crate) restriction_reason: Text,
    #[serde(default)]
    pub(crate) has_sensitive_content: bool,
}

/// `UserType` (`td_api.tl:796-820`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum UserType {
    #[serde(rename = "userTypeRegular")]
    Regular,
    #[serde(rename = "userTypeDeleted")]
    Deleted,
    /// A bot: its chat is held until T7 (H5).
    #[serde(rename = "userTypeBot")]
    Bot,
    #[default]
    #[serde(other)]
    Unknown,
}

/// `user` (`td_api.tl:2712`) as `updateUser` tells it (T3's people; the login's `getMe` reads
/// [`super::User`]).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct TgUser {
    #[serde(default)]
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) first_name: Text,
    #[serde(default)]
    pub(crate) last_name: Text,
    #[serde(default)]
    pub(crate) usernames: Option<Usernames>,
    /// Digits without `+`; empty when the user hides it.
    #[serde(default)]
    pub(crate) phone_number: Text,
    #[serde(default)]
    pub(crate) profile_photo: Option<PhotoInfo>,
    #[serde(default)]
    pub(crate) is_contact: bool,
    #[serde(default, rename = "type")]
    pub(crate) r#type: UserType,
    #[serde(default)]
    pub(crate) restriction_info: Option<RestrictionInfo>,
}

/// `basicGroup` (`td_api.tl:3013`): an upgrade names its supergroup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct BasicGroup {
    #[serde(default)]
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) member_count: i32,
    #[serde(default)]
    pub(crate) is_active: bool,
    #[serde(default)]
    pub(crate) upgraded_to_supergroup_id: i64,
}

/// `supergroup` (`td_api.tl:3056`): the chat's kind (supergroup, broadcast channel, forum,
/// direct-messages group) and whether it is public (an active username).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct Supergroup {
    #[serde(default)]
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) usernames: Option<Usernames>,
    #[serde(default)]
    pub(crate) member_count: i32,
    #[serde(default)]
    pub(crate) is_channel: bool,
    #[serde(default)]
    pub(crate) is_broadcast_group: bool,
    #[serde(default)]
    pub(crate) is_forum: bool,
    #[serde(default)]
    pub(crate) is_direct_messages_group: bool,
    #[serde(default)]
    pub(crate) restriction_info: Option<RestrictionInfo>,
}

// The chat updates (`td_api.tl:10845-11101`): each is deserialized straight from its line (the
// line's own `@type` is an unknown field, ignored).

/// `updateNewChat` (`td_api.tl:10845`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct NewChat {
    #[serde(default)]
    pub(crate) chat: Chat,
}

/// `updateChatTitle` (`td_api.tl:10848`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatTitle {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) title: Text,
}

/// `updateChatPhoto` (`td_api.tl:10851`): `None` = the photo was removed.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatPhoto {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) photo: Option<PhotoInfo>,
}

/// `updateChatLastMessage` (`td_api.tl:10869`): `last_message` null = unknown now.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatLastMessage {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) last_message: Option<Box<super::Message>>,
    #[serde(default)]
    pub(crate) positions: Vec<ChatPosition>,
}

/// `updateChatPosition` (`td_api.tl:10874`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatPositionUpdate {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) position: ChatPosition,
}

/// `updateChatAddedToList` / `updateChatRemovedFromList` (`td_api.tl:10877, 10880`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatInList {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) chat_list: ChatList,
}

/// `updateChatReadInbox` (`td_api.tl:10883`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatReadInbox {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) last_read_inbox_message_id: i64,
    #[serde(default)]
    pub(crate) unread_count: i32,
}

/// `updateChatReadOutbox` (`td_api.tl:10886`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatReadOutbox {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) last_read_outbox_message_id: i64,
}

/// `updateChatNotificationSettings` (`td_api.tl:10915`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatNotifications {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) notification_settings: ChatNotificationSettings,
}

/// `updateScopeNotificationSettings` (`td_api.tl:11031`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ScopeNotifications {
    #[serde(default)]
    pub(crate) scope: NotificationScope,
    #[serde(default)]
    pub(crate) notification_settings: ScopeNotificationSettings,
}

/// `updateChatIsMarkedAsUnread` (`td_api.tl:10951`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatMarkedUnread {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) is_marked_as_unread: bool,
}

/// `updateChatMessageAutoDeleteTime` (`td_api.tl:10912`): the chat's timer in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatTimer {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) message_auto_delete_time: i32,
}

/// `updateChatHasProtectedContent` (`td_api.tl:10945`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct ChatProtected {
    #[serde(default)]
    pub(crate) chat_id: i64,
    #[serde(default)]
    pub(crate) has_protected_content: bool,
}

/// `updateUser` (`td_api.tl:11095`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct UserUpdate {
    #[serde(default)]
    pub(crate) user: TgUser,
}

/// `updateBasicGroup` (`td_api.tl:11098`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct BasicGroupUpdate {
    #[serde(default)]
    pub(crate) basic_group: BasicGroup,
}

/// `updateSupergroup` (`td_api.tl:11101`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct SupergroupUpdate {
    #[serde(default)]
    pub(crate) supergroup: Supergroup,
}
