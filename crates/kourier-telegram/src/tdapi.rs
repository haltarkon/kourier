//! The subset of TDLib's JSON interface (td_api, the pinned 1.8.69) the login and the session read
//! (TG-2 brief §2, L-14; critic M14).
//!
//! - An update is parsed PROBE FIRST: its `@type` alone, then only a known type is deserialized
//!   (an internally tagged enum over every update would buffer each whole object; T3's message
//!   rates pay for that). Anything else is [`Update::Other`].
//! - Every field has `#[serde(default)]` (TDLib adds fields with every version) and nothing denies
//!   unknown fields; each tagged enum has a `#[serde(other)]` unit variant for a type this build
//!   does not know.
//! - td_json writes an `int64` as a JSON STRING (`optionValueInteger.value`, `my_id`), an `int53`
//!   or `int32` as a number; [`int64`] takes both.
//! - `Debug` never prints a phone number, a name, a hint, a pattern, a link or a code word: the
//!   types that hold one have a hand-written `Debug` with the variant name and numbers only.
//! - TG-3 C1: the receive updates (brief §2.2) are [`Update::Receive`]; their objects live in
//!   `chat` and `message`. Every free text there (a title, a name, a number, a username, a message
//!   text, a link) is a [`Text`] and every raw part (a message's content) a [`Raw`]: their `Debug`
//!   prints nothing of the value, so the derived `Debug`s of those types print ids, kinds and counts.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

mod chat;
mod message;

pub(crate) use chat::{
    BasicGroup, BasicGroupUpdate, Chat, ChatInList, ChatLastMessage, ChatList, ChatMarkedUnread,
    ChatNotifications, ChatPhoto, ChatPosition, ChatPositionUpdate, ChatProtected, ChatReadInbox,
    ChatReadOutbox, ChatTimer, ChatTitle, ChatType, File, NewChat, NotificationScope,
    RestrictionInfo, ScopeNotifications, Supergroup, SupergroupUpdate, TgUser, UserType,
    UserUpdate,
};
pub(crate) use message::{
    ChatAction, ChatActionUpdate, DeleteMessages, FormattedText, Message, MessageContent,
    MessageEdited, MessageSender, NewMessage, ReplyTo, SelfDestructType, TextEntity,
    TextEntityType,
};
// Names only the tests build values of.
#[cfg(test)]
pub(crate) use message::{SchedulingState, SendingState};

/// Free text of TDLib (a title, a name, a number, a username, a message text, a link, a reason):
/// its `Debug` prints NOTHING of it (the log rule), so a derived `Debug` of a type holding it stays
/// safe. Read it with [`Text::as_str`].
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct Text(String);

impl Text {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Text(..)")
    }
}

/// A raw td_api object kept for a later mapping (a message's content, its forward info): its
/// `Debug` prints the object's `@type` only.
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct Raw(Value);

impl Raw {
    /// The object's `@type` (`""` when it has none).
    pub(crate) fn type_name(&self) -> &str {
        self.0
            .get("@type")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    pub(crate) fn value(&self) -> &Value {
        &self.0
    }
}

impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Raw({})", self.type_name())
    }
}

/// An `int64` of td_json: a JSON string of digits (what TDLib writes) or a number.
fn int64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Number(i64),
        Text(String),
    }
    match Either::deserialize(d)? {
        Either::Number(n) => Ok(n),
        // The parse error names no digits.
        Either::Text(s) => s
            .parse::<i64>()
            .map_err(|_| serde::de::Error::custom("an int64 that is no number")),
    }
}

/// A td_api `double` of seconds (a time left) as whole seconds, rounded UP so a timer never ends
/// early; a negative, non-finite or huge value is 0 (`Update` stays `Eq`: no float is kept).
fn secs<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = f64::deserialize(d)?;
    Ok(if v.is_finite() && v > 0.0 && v < 1e12 {
        // In range: at most 1e12, so the cast is exact enough and cannot overflow.
        v.ceil() as i64
    } else {
        0
    })
}

/// `AuthenticationCodeType` (`td_api.tl:29-70`): how the code was sent.
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum CodeType {
    /// To another session of the account (the usual way for an unofficial app).
    #[serde(rename = "authenticationCodeTypeTelegramMessage")]
    TelegramMessage {
        #[serde(default)]
        length: i32,
    },
    #[serde(rename = "authenticationCodeTypeSms")]
    Sms {
        #[serde(default)]
        length: i32,
    },
    /// A word by SMS; `first_letter` is a part of the code (never printed).
    #[serde(rename = "authenticationCodeTypeSmsWord")]
    SmsWord {
        #[serde(default)]
        first_letter: String,
    },
    /// A phrase by SMS; `first_word` is a part of the code (never printed).
    #[serde(rename = "authenticationCodeTypeSmsPhrase")]
    SmsPhrase {
        #[serde(default)]
        first_word: String,
    },
    #[serde(rename = "authenticationCodeTypeCall")]
    Call {
        #[serde(default)]
        length: i32,
    },
    /// Entered automatically by an official app: Kourier cannot.
    #[serde(rename = "authenticationCodeTypeFlashCall")]
    FlashCall,
    /// Only with `allow_missed_call`, which Kourier never sets.
    #[serde(rename = "authenticationCodeTypeMissedCall")]
    MissedCall {
        #[serde(default)]
        length: i32,
    },
    /// The code on fragment.com (a foreign link, not kept).
    #[serde(rename = "authenticationCodeTypeFragment")]
    Fragment {
        #[serde(default)]
        length: i32,
    },
    #[serde(rename = "authenticationCodeTypeFirebaseAndroid")]
    FirebaseAndroid,
    #[serde(rename = "authenticationCodeTypeFirebaseIos")]
    FirebaseIos,
    /// A type this build does not know.
    #[default]
    #[serde(other)]
    Unknown,
}

impl CodeType {
    /// The code's length when TDLib says it (0: unknown).
    pub(crate) fn length(&self) -> i32 {
        match self {
            Self::TelegramMessage { length }
            | Self::Sms { length }
            | Self::Call { length }
            | Self::MissedCall { length }
            | Self::Fragment { length } => *length,
            Self::SmsWord { .. }
            | Self::SmsPhrase { .. }
            | Self::FlashCall
            | Self::FirebaseAndroid
            | Self::FirebaseIos
            | Self::Unknown => 0,
        }
    }
}

impl fmt::Debug for CodeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::TelegramMessage { .. } => "TelegramMessage",
            Self::Sms { .. } => "Sms",
            Self::SmsWord { .. } => "SmsWord",
            Self::SmsPhrase { .. } => "SmsPhrase",
            Self::Call { .. } => "Call",
            Self::FlashCall => "FlashCall",
            Self::MissedCall { .. } => "MissedCall",
            Self::Fragment { .. } => "Fragment",
            Self::FirebaseAndroid => "FirebaseAndroid",
            Self::FirebaseIos => "FirebaseIos",
            Self::Unknown => "Unknown",
        };
        write!(f, "{name}(length {})", self.length())
    }
}

/// `authenticationCodeInfo` (`td_api.tl:78`).
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct AuthenticationCodeInfo {
    /// The number the code went to (never printed).
    #[serde(default)]
    pub(crate) phone_number: String,
    #[serde(default, rename = "type")]
    pub(crate) r#type: CodeType,
    /// How a resend would come; `None`: no resend.
    #[serde(default)]
    pub(crate) next_type: Option<CodeType>,
    /// Seconds before a resend is allowed.
    #[serde(default)]
    pub(crate) timeout: i32,
}

impl fmt::Debug for AuthenticationCodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticationCodeInfo")
            .field("type", &self.r#type)
            .field("next_type", &self.next_type)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// `emailAddressAuthenticationCodeInfo` (`td_api.tl:83`).
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct EmailCodeInfo {
    /// A pattern of the address (never printed).
    #[serde(default)]
    pub(crate) email_address_pattern: String,
    /// 0 when unknown.
    #[serde(default)]
    pub(crate) length: i32,
}

impl fmt::Debug for EmailCodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmailCodeInfo")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

/// `AuthorizationState` (`td_api.tl:198-251`): the fields the login machine reads.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum AuthorizationState {
    #[serde(rename = "authorizationStateWaitTdlibParameters")]
    WaitTdlibParameters,
    #[serde(rename = "authorizationStateWaitPhoneNumber")]
    WaitPhoneNumber,
    /// The QR code's `tg://login?token=` link (a login secret: never printed).
    #[serde(rename = "authorizationStateWaitOtherDeviceConfirmation")]
    WaitOtherDeviceConfirmation {
        #[serde(default)]
        link: String,
    },
    #[serde(rename = "authorizationStateWaitCode")]
    WaitCode {
        #[serde(default)]
        code_info: AuthenticationCodeInfo,
    },
    /// The 2FA password; the hint is the account owner's own words (never printed or logged).
    #[serde(rename = "authorizationStateWaitPassword")]
    WaitPassword {
        #[serde(default)]
        password_hint: String,
        #[serde(default)]
        has_recovery_email_address: bool,
        #[serde(default)]
        recovery_email_address_pattern: String,
    },
    #[serde(rename = "authorizationStateWaitEmailAddress")]
    WaitEmailAddress {
        #[serde(default)]
        allow_apple_id: bool,
        #[serde(default)]
        allow_google_id: bool,
    },
    #[serde(rename = "authorizationStateWaitEmailCode")]
    WaitEmailCode {
        #[serde(default)]
        code_info: EmailCodeInfo,
    },
    /// A new account: Kourier never registers one (refused, TSC H6).
    #[serde(rename = "authorizationStateWaitRegistration")]
    WaitRegistration,
    /// A login that needs a purchase: refused.
    #[serde(rename = "authorizationStateWaitPremiumPurchase")]
    WaitPremiumPurchase,
    #[serde(rename = "authorizationStateReady")]
    Ready,
    #[serde(rename = "authorizationStateLoggingOut")]
    LoggingOut,
    #[serde(rename = "authorizationStateClosing")]
    Closing,
    #[serde(rename = "authorizationStateClosed")]
    Closed,
    /// A state this build does not know.
    #[serde(other)]
    Unknown,
}

impl AuthorizationState {
    /// The state of a `getAuthorizationState` answer (an `AuthorizationState` object itself);
    /// `Unknown` when it is none.
    pub(crate) fn from_value(v: Value) -> Self {
        serde_json::from_value(v).unwrap_or(Self::Unknown)
    }

    fn name(&self) -> &'static str {
        match self {
            Self::WaitTdlibParameters => "WaitTdlibParameters",
            Self::WaitPhoneNumber => "WaitPhoneNumber",
            Self::WaitOtherDeviceConfirmation { .. } => "WaitOtherDeviceConfirmation",
            Self::WaitCode { .. } => "WaitCode",
            Self::WaitPassword { .. } => "WaitPassword",
            Self::WaitEmailAddress { .. } => "WaitEmailAddress",
            Self::WaitEmailCode { .. } => "WaitEmailCode",
            Self::WaitRegistration => "WaitRegistration",
            Self::WaitPremiumPurchase => "WaitPremiumPurchase",
            Self::Ready => "Ready",
            Self::LoggingOut => "LoggingOut",
            Self::Closing => "Closing",
            Self::Closed => "Closed",
            Self::Unknown => "Unknown",
        }
    }
}

impl fmt::Debug for AuthorizationState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WaitCode { code_info } => write!(f, "WaitCode({code_info:?})"),
            Self::WaitEmailCode { code_info } => write!(f, "WaitEmailCode({code_info:?})"),
            other => f.write_str(other.name()),
        }
    }
}

/// `ConnectionState` (`td_api.tl:10286-10298`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum ConnectionState {
    #[serde(rename = "connectionStateWaitingForNetwork")]
    WaitingForNetwork,
    /// Behind the gate this is the normal "connecting" (TDLib's proxy is the gate).
    #[serde(rename = "connectionStateConnectingToProxy")]
    ConnectingToProxy,
    #[serde(rename = "connectionStateConnecting")]
    Connecting,
    #[serde(rename = "connectionStateUpdating")]
    Updating,
    #[serde(rename = "connectionStateReady")]
    Ready,
    #[serde(other)]
    Unknown,
}

/// `OptionValue` (`td_api.tl:9235-9244`).
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "@type")]
pub(crate) enum OptionValue {
    #[serde(rename = "optionValueBoolean")]
    Boolean {
        #[serde(default)]
        value: bool,
    },
    #[serde(rename = "optionValueEmpty")]
    Empty,
    /// An int64: a JSON string in td_json.
    #[serde(rename = "optionValueInteger")]
    Integer {
        #[serde(default, deserialize_with = "int64")]
        value: i64,
    },
    /// May hold a host name or a link (never printed).
    #[serde(rename = "optionValueString")]
    String {
        #[serde(default)]
        value: String,
    },
    #[serde(other)]
    Unknown,
}

impl fmt::Debug for OptionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boolean { value } => write!(f, "Boolean({value})"),
            Self::Empty => f.write_str("Empty"),
            // An option's number may be an id (`my_id`): not printed either.
            Self::Integer { .. } => f.write_str("Integer(..)"),
            Self::String { .. } => f.write_str("String(..)"),
            Self::Unknown => f.write_str("Unknown"),
        }
    }
}

/// `updateFreezeState` (`td_api.tl:11347`).
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct FreezeState {
    #[serde(default)]
    pub(crate) is_frozen: bool,
    #[serde(default)]
    pub(crate) freezing_date: i32,
    #[serde(default)]
    pub(crate) deletion_date: i32,
    /// Telegram's appeal page (not kept by TG-2: the appeal is made in the Telegram app).
    #[serde(default)]
    pub(crate) appeal_link: String,
}

impl fmt::Debug for FreezeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FreezeState")
            .field("is_frozen", &self.is_frozen)
            .field("freezing_date", &self.freezing_date)
            .field("deletion_date", &self.deletion_date)
            .finish_non_exhaustive()
    }
}

/// `user` (`td_api.tl:2712`): what the identity checks and `AccountReady` read (`getMe`).
#[derive(Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct User {
    /// An int53: a JSON number.
    #[serde(default)]
    pub(crate) id: i64,
    #[serde(default)]
    pub(crate) first_name: String,
    #[serde(default)]
    pub(crate) last_name: String,
    /// Digits without `+` (never printed).
    #[serde(default)]
    pub(crate) phone_number: String,
}

impl User {
    /// The user of a `getMe` answer; `None` when it is no `user` object.
    pub(crate) fn from_value(v: Value) -> Option<Self> {
        (v.get("@type").and_then(Value::as_str) == Some("user"))
            .then(|| serde_json::from_value(v).ok())
            .flatten()
    }
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("User { .. }")
    }
}

/// The updates TG-2 acts on; [`parse_update`] makes them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Update {
    AuthorizationState(AuthorizationState),
    ConnectionState(ConnectionState),
    FreezeState(FreezeState),
    /// `updateOption`.
    Option {
        name: String,
        value: OptionValue,
    },
    /// TG-3: an update of the chat and message machines (brief §2.2).
    Receive(Receive),
    /// Any other update (not deserialized beyond its `@type`).
    Other,
}

/// The receive updates of TG-3 (brief §2.2), one variant per `@type`, each deserialized straight
/// from its line. `updateChatFolders` (T8), `updateUserFullInfo`, the send results (T5) and the
/// counters stay [`Update::Other`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Receive {
    NewChat(Box<Chat>),
    ChatTitle(ChatTitle),
    ChatPhoto(ChatPhoto),
    ChatLastMessage(Box<ChatLastMessage>),
    ChatPosition(ChatPositionUpdate),
    ChatAddedToList(ChatInList),
    ChatRemovedFromList(ChatInList),
    ChatReadInbox(ChatReadInbox),
    ChatReadOutbox(ChatReadOutbox),
    ChatNotificationSettings(ChatNotifications),
    ScopeNotificationSettings(ScopeNotifications),
    ChatIsMarkedAsUnread(ChatMarkedUnread),
    ChatMessageAutoDeleteTime(ChatTimer),
    ChatHasProtectedContent(ChatProtected),
    ChatAction(ChatActionUpdate),
    User(Box<TgUser>),
    BasicGroup(BasicGroup),
    Supergroup(Box<Supergroup>),
    NewMessage(Box<Message>),
    MessageContent(Box<MessageContent>),
    MessageEdited(MessageEdited),
    DeleteMessages(DeleteMessages),
}

/// The `@type`s [`parse_update`] makes a [`Receive`] of (a test checks every one parses).
pub(crate) const RECEIVE_TYPES: [&str; 22] = [
    "updateNewChat",
    "updateChatTitle",
    "updateChatPhoto",
    "updateChatLastMessage",
    "updateChatPosition",
    "updateChatAddedToList",
    "updateChatRemovedFromList",
    "updateChatReadInbox",
    "updateChatReadOutbox",
    "updateChatNotificationSettings",
    "updateScopeNotificationSettings",
    "updateChatIsMarkedAsUnread",
    "updateChatMessageAutoDeleteTime",
    "updateChatHasProtectedContent",
    "updateChatAction",
    "updateUser",
    "updateBasicGroup",
    "updateSupergroup",
    "updateNewMessage",
    "updateMessageContent",
    "updateMessageEdited",
    "updateDeleteMessages",
];

/// A receive update of a known `@type`: `None` when the line does not parse as it.
fn receive(ty: &str, json: &str) -> Option<Receive> {
    fn de<T: DeserializeOwned>(json: &str) -> Option<T> {
        serde_json::from_str(json).ok()
    }
    Some(match ty {
        "updateNewChat" => Receive::NewChat(Box::new(de::<NewChat>(json)?.chat)),
        "updateChatTitle" => Receive::ChatTitle(de(json)?),
        "updateChatPhoto" => Receive::ChatPhoto(de(json)?),
        "updateChatLastMessage" => Receive::ChatLastMessage(Box::new(de(json)?)),
        "updateChatPosition" => Receive::ChatPosition(de(json)?),
        "updateChatAddedToList" => Receive::ChatAddedToList(de(json)?),
        "updateChatRemovedFromList" => Receive::ChatRemovedFromList(de(json)?),
        "updateChatReadInbox" => Receive::ChatReadInbox(de(json)?),
        "updateChatReadOutbox" => Receive::ChatReadOutbox(de(json)?),
        "updateChatNotificationSettings" => Receive::ChatNotificationSettings(de(json)?),
        "updateScopeNotificationSettings" => Receive::ScopeNotificationSettings(de(json)?),
        "updateChatIsMarkedAsUnread" => Receive::ChatIsMarkedAsUnread(de(json)?),
        "updateChatMessageAutoDeleteTime" => Receive::ChatMessageAutoDeleteTime(de(json)?),
        "updateChatHasProtectedContent" => Receive::ChatHasProtectedContent(de(json)?),
        "updateChatAction" => Receive::ChatAction(de(json)?),
        "updateUser" => Receive::User(Box::new(de::<UserUpdate>(json)?.user)),
        "updateBasicGroup" => Receive::BasicGroup(de::<BasicGroupUpdate>(json)?.basic_group),
        "updateSupergroup" => {
            Receive::Supergroup(Box::new(de::<SupergroupUpdate>(json)?.supergroup))
        }
        "updateNewMessage" => Receive::NewMessage(Box::new(de::<NewMessage>(json)?.message)),
        "updateMessageContent" => Receive::MessageContent(Box::new(de(json)?)),
        "updateMessageEdited" => Receive::MessageEdited(de(json)?),
        "updateDeleteMessages" => Receive::DeleteMessages(de(json)?),
        _ => return None,
    })
}

impl Update {
    /// The own user id of an `updateOption` `my_id` (an int64 option: a JSON string).
    pub(crate) fn my_id(&self) -> Option<i64> {
        match self {
            Self::Option {
                name,
                value: OptionValue::Integer { value },
            } if name == "my_id" => Some(*value),
            _ => None,
        }
    }
}

/// One update of TDLib, probe first: `None` when the line is no JSON object with a string `@type`,
/// or a known type that does not parse (a TDLib this build was not written for); `Other` for every
/// type outside the list.
pub(crate) fn parse_update(json: &str) -> Option<Update> {
    #[derive(Deserialize)]
    struct Probe {
        #[serde(rename = "@type")]
        ty: String,
    }
    #[derive(Deserialize)]
    struct Auth {
        authorization_state: AuthorizationState,
    }
    #[derive(Deserialize)]
    struct Conn {
        state: ConnectionState,
    }
    #[derive(Deserialize)]
    struct Opt {
        #[serde(default)]
        name: String,
        value: OptionValue,
    }
    let probe: Probe = serde_json::from_str(json).ok()?;
    Some(match probe.ty.as_str() {
        "updateAuthorizationState" => {
            Update::AuthorizationState(serde_json::from_str::<Auth>(json).ok()?.authorization_state)
        }
        "updateConnectionState" => {
            Update::ConnectionState(serde_json::from_str::<Conn>(json).ok()?.state)
        }
        "updateFreezeState" => Update::FreezeState(serde_json::from_str::<FreezeState>(json).ok()?),
        "updateOption" => {
            let o: Opt = serde_json::from_str(json).ok()?;
            Update::Option {
                name: o.name,
                value: o.value,
            }
        }
        // A receive type that does not parse is `None`, like the types above.
        ty if RECEIVE_TYPES.contains(&ty) => Update::Receive(receive(ty, json)?),
        _ => Update::Other,
    })
}

#[cfg(test)]
mod tests;
