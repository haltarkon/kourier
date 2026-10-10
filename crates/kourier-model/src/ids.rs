//! Identifiers (architecture §3.1): core-local rowids, the account uuid, network ids with
//! versioned string grammars, structured message ids and message references.
//!
//! # String grammars
//!
//! Every network id string starts with a prefix that names its grammar. A prefix is frozen once
//! data exists; a changed grammar gets a new prefix (§3.1 rules).
//!
//! | Type | Grammar | Example |
//! |---|---|---|
//! | [`UserId`] | `<scheme>:<value>` | `aci:5f3c…`, `e164:+4915…`, `lid:123`, `threema:ECHOECHO` |
//! | [`ConvKey`] | `direct:<user id>` / `group:<id>` / `broadcast:<id>` / `channel:<id>`, where `<id>` is `<scheme>:<value>` | `group:gv2:AbC…` |
//! | [`NetMsgId`] | `m1:<local>` (own) / `m1:<local>@<user id>` | `m1:1700000000000@aci:5f3c…` |
//!
//! `<scheme>` is `[a-z][a-z0-9-]{0,15}`; `<value>` is non-empty and has no control or whitespace
//! characters. In a [`NetMsgId`] string `%` and `@` inside `<local>` are written as `%25` and
//! `%40`. Ids are at most [`MAX_ID_LEN`] bytes.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Maximum length in bytes of a [`UserId`], of the id inside a [`ConvKey`] and of a message
/// `local` part.
pub const MAX_ID_LEN: usize = 256;
/// Maximum length in bytes of an id scheme (`aci`, `gv2`, …).
pub const MAX_SCHEME_LEN: usize = 16;

/// Why an id string was rejected. Never contains the input (it may be a phone number).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The id or one of its parts is empty.
    #[error("empty id")]
    Empty,
    /// The id is longer than [`MAX_ID_LEN`] bytes.
    #[error("id longer than {MAX_ID_LEN} bytes")]
    TooLong,
    /// Missing `<scheme>:` prefix, or the scheme has invalid characters.
    #[error("missing or invalid id scheme")]
    BadScheme,
    /// The id contains a control or whitespace character, or a bad `%` escape.
    #[error("invalid character in id")]
    BadChar,
    /// The grammar prefix (`direct:`, `m1:`, …) is unknown.
    #[error("unknown id prefix")]
    UnknownPrefix,
}

macro_rules! local_id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

local_id!(
    /// `core.db` rowid of an account.
    AccountId
);
local_id!(
    /// `core.db` rowid of a conversation.
    ConversationId
);
local_id!(
    /// `core.db` rowid of a message; sent to backends only as [`LocalMsgId`].
    MessageId
);
local_id!(
    /// `core.db` rowid of a contact.
    ContactId
);
local_id!(
    /// `core.db` rowid of an attachment; the correlation handle of a download.
    AttachmentId
);
local_id!(
    /// Id of a durable command (`pending_commands` rowid, §4.8).
    CmdId
);
local_id!(
    /// Id of one `DownloadAttachment` request (`attachment_downloads` rowid, never reused):
    /// echoed in `AttachmentDone`, so a late answer to an earlier request of the same
    /// attachment cannot finish or fail a newer one.
    DownloadId
);

/// Opaque correlation handle for an own message, given to backends (= [`MessageId`]).
pub type LocalMsgId = MessageId;

/// Stable account uuid: names `accounts/<uuid>/` and is recorded in `key.wrap` (§5.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountUuid(pub Uuid);

impl AccountUuid {
    /// A new random (v4) uuid.
    pub fn new_random() -> Self {
        Self(Uuid::new_v4())
    }

    /// The first 8 hex digits, for thread names (`signal-<uuid8>`) and logs.
    pub fn short(&self) -> String {
        let mut s = self.0.simple().to_string();
        s.truncate(8);
        s
    }
}

impl fmt::Display for AccountUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

impl FromStr for AccountUuid {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

/// The networks Kourier knows. Stored as TEXT by [`NetworkKind::as_str`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub enum NetworkKind {
    /// Signal (M1).
    Signal,
    /// WhatsApp (later, §7.1).
    WhatsApp,
    /// Threema (later, §7.2).
    Threema,
    /// The scripted test backend (`kourier-fake`).
    Fake,
}

impl NetworkKind {
    /// Every kind, in declaration order.
    pub const ALL: [NetworkKind; 4] = [Self::Signal, Self::WhatsApp, Self::Threema, Self::Fake];

    /// The stored name (same as the serde name).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Signal => "Signal",
            Self::WhatsApp => "WhatsApp",
            Self::Threema => "Threema",
            Self::Fake => "Fake",
        }
    }
}

impl fmt::Display for NetworkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NetworkKind {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or(IdError::UnknownPrefix)
    }
}

/// Checks the `<scheme>:<value>` grammar shared by user ids and conversation ids.
fn validate_scoped(s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty);
    }
    if s.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    let (scheme, value) = s.split_once(':').ok_or(IdError::BadScheme)?;
    validate_scheme(scheme)?;
    if value.is_empty() {
        return Err(IdError::Empty);
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(IdError::BadChar);
    }
    Ok(())
}

fn validate_scheme(scheme: &str) -> Result<(), IdError> {
    let mut chars = scheme.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if first_ok && rest_ok && scheme.len() <= MAX_SCHEME_LEN {
        Ok(())
    } else {
        Err(IdError::BadScheme)
    }
}

/// Checks a message `local` part: non-empty, at most [`MAX_ID_LEN`] bytes, no control chars.
pub fn validate_local(local: &str) -> Result<(), IdError> {
    if local.is_empty() {
        return Err(IdError::Empty);
    }
    if local.len() > MAX_ID_LEN {
        return Err(IdError::TooLong);
    }
    if local.chars().any(char::is_control) {
        return Err(IdError::BadChar);
    }
    Ok(())
}

/// Serde through the string grammar (`parse` on the way in, so stored data is validated).
macro_rules! string_serde {
    ($ty:ty, $encode:expr) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let encode: fn(&$ty) -> std::borrow::Cow<'_, str> = $encode;
                s.serialize_str(&encode(self))
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                <$ty>::parse(&s).map_err(serde::de::Error::custom)
            }
        }

        impl FromStr for $ty {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                <$ty>::parse(s)
            }
        }
    };
}

/// A user on a network, as the backend names it (`<scheme>:<value>`, see the module docs).
/// Signal: `aci:<uuid>`, `pni:<uuid>`, `e164:+<digits>`; WhatsApp: `lid:<n>`, `pn:<n>`;
/// Threema: `threema:<ID>`. The core never parses it beyond the grammar.
///
/// `Debug` prints the log-safe form ([`UserId::log_safe`]); the full id is [`UserId::as_str`].
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(Arc<str>);

impl UserId {
    /// Parses and validates a user id string.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        validate_scoped(s)?;
        Ok(Self(Arc::from(s)))
    }

    /// Builds `<scheme>:<value>` and validates it; afterwards [`UserId::scheme`] is `scheme`
    /// and [`UserId::value`] is `value`.
    pub fn from_parts(scheme: &str, value: &str) -> Result<Self, IdError> {
        validate_scheme(scheme)?;
        Self::parse(&format!("{scheme}:{value}"))
    }

    /// The full id string. Not log-safe.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The grammar prefix (`aci`, `e164`, …).
    pub fn scheme(&self) -> &str {
        self.0.split_once(':').map_or("", |(s, _)| s)
    }

    /// The part after the scheme. Not log-safe.
    pub fn value(&self) -> &str {
        self.0.split_once(':').map_or("", |(_, v)| v)
    }

    /// Log-safe display: `aci:…1f2e` (last 4 characters), phone numbers fully hidden (§9.1).
    pub fn log_safe(&self) -> crate::log::LogSafeUserId<'_> {
        crate::log::LogSafeUserId(self)
    }
}

impl fmt::Debug for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserId({})", self.log_safe())
    }
}

string_serde!(UserId, |u| std::borrow::Cow::Borrowed(u.as_str()));

/// A conversation on a network (§3.1). Signal group: `Group("gv2:<base64url group id>")`, the
/// id, never the master key. `Broadcast` and `Channel` are reserved for Threema distribution
/// lists and WhatsApp newsletters.
///
/// Stored as TEXT by [`ConvKey::encode`]; `Debug` is log-safe.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConvKey {
    /// A 1:1 chat with this user.
    Direct(UserId),
    /// A group (`<scheme>:<value>`).
    Group(Arc<str>),
    /// A distribution list (`<scheme>:<value>`).
    Broadcast(Arc<str>),
    /// A channel/newsletter (`<scheme>:<value>`).
    Channel(Arc<str>),
}

impl ConvKey {
    const DIRECT: &'static str = "direct";
    const GROUP: &'static str = "group";
    const BROADCAST: &'static str = "broadcast";
    const CHANNEL: &'static str = "channel";

    /// Parses the string form (`direct:aci:…`, `group:gv2:…`, …).
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let (tag, rest) = s.split_once(':').ok_or(IdError::UnknownPrefix)?;
        let id = |rest: &str| validate_scoped(rest).map(|()| Arc::<str>::from(rest));
        match tag {
            Self::DIRECT => UserId::parse(rest).map(Self::Direct),
            Self::GROUP => id(rest).map(Self::Group),
            Self::BROADCAST => id(rest).map(Self::Broadcast),
            Self::CHANNEL => id(rest).map(Self::Channel),
            _ => Err(IdError::UnknownPrefix),
        }
    }

    /// The string form; `parse(encode(k)) == k` for every valid key.
    pub fn encode(&self) -> String {
        let (tag, id) = self.parts();
        format!("{tag}:{id}")
    }

    /// Checks the grammar of the inner id (variants can be built without [`ConvKey::parse`]).
    pub fn validate(&self) -> Result<(), IdError> {
        match self {
            Self::Direct(u) => validate_scoped(u.as_str()),
            Self::Group(id) | Self::Broadcast(id) | Self::Channel(id) => validate_scoped(id),
        }
    }

    /// The conversation kind this key implies.
    pub fn kind(&self) -> crate::conversation::ConvKind {
        use crate::conversation::ConvKind;
        match self {
            Self::Direct(_) => ConvKind::Direct,
            Self::Group(_) => ConvKind::Group,
            Self::Broadcast(_) => ConvKind::Broadcast,
            Self::Channel(_) => ConvKind::Channel,
        }
    }

    /// The peer of a direct conversation.
    pub fn direct_peer(&self) -> Option<&UserId> {
        match self {
            Self::Direct(u) => Some(u),
            _ => None,
        }
    }

    /// Log-safe display: `conv#a1b2` (a stable 16-bit hash of the key, §9.1).
    pub fn log_safe(&self) -> crate::log::LogSafeConvKey<'_> {
        crate::log::LogSafeConvKey(self)
    }

    fn parts(&self) -> (&'static str, &str) {
        match self {
            Self::Direct(u) => (Self::DIRECT, u.as_str()),
            Self::Group(id) => (Self::GROUP, id),
            Self::Broadcast(id) => (Self::BROADCAST, id),
            Self::Channel(id) => (Self::CHANNEL, id),
        }
    }
}

impl fmt::Debug for ConvKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (tag, _) = self.parts();
        write!(f, "ConvKey({tag} {})", self.log_safe())
    }
}

string_serde!(ConvKey, |k| std::borrow::Cow::Owned(k.encode()));

/// How a network scopes message ids (§3.1 matching scope). Stored as `messages.net_scope`.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub enum MsgIdScope {
    /// `(account, author, local)` is unique account-wide (Signal).
    #[default]
    AccountAuthor,
    /// `(conversation, local)` is unique; authors are compared only when both sides carry one
    /// (WhatsApp, Threema).
    Conversation,
}

impl MsgIdScope {
    /// The `messages.net_scope` column value (§5.4): 0 = AccountAuthor, 1 = Conversation.
    pub fn db_code(self) -> i64 {
        match self {
            Self::AccountAuthor => 0,
            Self::Conversation => 1,
        }
    }

    /// Inverse of [`MsgIdScope::db_code`].
    pub fn from_db_code(code: i64) -> Option<Self> {
        match code {
            0 => Some(Self::AccountAuthor),
            1 => Some(Self::Conversation),
            _ => None,
        }
    }
}

/// A network message id (§3.1). `author: None` = our own account, whichever own id the network
/// used. Stored as two columns: `net_author` (`''` = own) and `net_local`. The core never parses
/// `local` (Signal: sent timestamp in ms; WhatsApp: stanza id; Threema: u64 hex).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NetMsgId {
    /// The author; `None` for own messages.
    pub author: Option<UserId>,
    /// The network-local part.
    pub local: Arc<str>,
}

impl NetMsgId {
    const PREFIX: &'static str = "m1:";

    /// Id of an own message.
    pub fn own(local: impl Into<Arc<str>>) -> Self {
        Self {
            author: None,
            local: local.into(),
        }
    }

    /// Id of a message by `author`.
    pub fn remote(author: UserId, local: impl Into<Arc<str>>) -> Self {
        Self {
            author: Some(author),
            local: local.into(),
        }
    }

    /// Parses `m1:<local>` or `m1:<local>@<user id>`.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let rest = s.strip_prefix(Self::PREFIX).ok_or(IdError::UnknownPrefix)?;
        let (local, author) = match rest.split_once('@') {
            Some((l, a)) => (l, Some(UserId::parse(a)?)),
            None => (rest, None),
        };
        let local = unescape_local(local)?;
        validate_local(&local)?;
        Ok(Self {
            author,
            local: local.into(),
        })
    }

    /// The string form; `parse(encode(id)) == id` for every valid id.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.local.len() + 48);
        out.push_str(Self::PREFIX);
        for c in self.local.chars() {
            match c {
                '%' => out.push_str("%25"),
                '@' => out.push_str("%40"),
                c => out.push(c),
            }
        }
        if let Some(a) = &self.author {
            out.push('@');
            out.push_str(a.as_str());
        }
        out
    }

    /// Checks `local` and the author (fields are public, so ids can be built unchecked).
    pub fn validate(&self) -> Result<(), IdError> {
        validate_local(&self.local)?;
        match &self.author {
            Some(a) => validate_scoped(a.as_str()),
            None => Ok(()),
        }
    }

    /// The `net_author` column value: `''` for own messages.
    pub fn author_column(&self) -> &str {
        self.author.as_ref().map_or("", UserId::as_str)
    }

    /// Log-safe display (`m1:<local>@aci:…1f2e`).
    pub fn log_safe(&self) -> crate::log::LogSafeNetMsgId<'_> {
        crate::log::LogSafeNetMsgId(self)
    }
}

fn unescape_local(s: &str) -> Result<String, IdError> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let esc = rest.get(i..i + 3).ok_or(IdError::BadChar)?;
        out.push(match esc {
            "%25" => '%',
            "%40" => '@',
            _ => return Err(IdError::BadChar),
        });
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Ok(out)
}

impl fmt::Debug for NetMsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NetMsgId({})", self.log_safe())
    }
}

string_serde!(NetMsgId, |m| std::borrow::Cow::Owned(m.encode()));

/// Who wrote the message a [`MsgTarget`] points at.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub enum TargetAuthor {
    /// Our own account.
    Own,
    /// This user.
    User(UserId),
    /// The reference carries no sender (Threema reactions, quotes and receipts); the core
    /// resolves it by `local` within the conversation.
    Unknown,
}

/// A reference to a message (§3.1): the target of edits, deletes, reactions, receipts, read
/// markers and quotes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct MsgTarget {
    /// The conversation, when the network says; `None` = look up account-wide (only for
    /// [`MsgIdScope::AccountAuthor`]).
    ///
    /// A conversation that is stated binds: the reference is to a message of that
    /// conversation and resolves to no other, also where ids are account-wide. A backend
    /// states the conversation a modifier (edit, delete, reaction) arrived in, in its
    /// canonical form, and leaves it out only where the network sends none (Signal receipts
    /// and read syncs). That is what keeps a modifier sent in one chat from changing a
    /// message of another.
    #[serde(default)]
    pub conv: Option<ConvKey>,
    /// The author of the target.
    pub author: TargetAuthor,
    /// The `local` part of the target's id.
    pub local: Arc<str>,
    /// Other `local` values of the same message (none are known today).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternates: Vec<Arc<str>>,
}

impl MsgTarget {
    /// A reference to the message with id `id`.
    pub fn to_id(conv: Option<ConvKey>, id: &NetMsgId) -> Self {
        Self {
            conv,
            author: match &id.author {
                Some(u) => TargetAuthor::User(u.clone()),
                None => TargetAuthor::Own,
            },
            local: id.local.clone(),
            alternates: Vec::new(),
        }
    }

    /// The referenced id, when the author is known.
    pub fn net_id(&self) -> Option<NetMsgId> {
        let author = match &self.author {
            TargetAuthor::Own => None,
            TargetAuthor::User(u) => Some(u.clone()),
            TargetAuthor::Unknown => return None,
        };
        Some(NetMsgId {
            author,
            local: self.local.clone(),
        })
    }

    /// Whether `id` matches this reference by local (or an alternate) and, when known, author.
    /// Alias resolution (PNI/ACI, PN/LID) is the core's job and happens before this.
    pub fn matches(&self, id: &NetMsgId) -> bool {
        let local_ok = self.local == id.local || self.alternates.contains(&id.local);
        let author_ok = match (&self.author, &id.author) {
            (TargetAuthor::Unknown, _) => true,
            (TargetAuthor::Own, None) => true,
            (TargetAuthor::User(a), Some(b)) => a == b,
            _ => false,
        };
        local_ok && author_ok
    }
}

/// Opaque backend bytes the core stores verbatim (attachment pointers, raw payloads, per-row
/// metadata). `v` versions the backend's payload format. `Debug` prints only the length.
#[derive(Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct BackendBlob {
    /// Backend-defined payload version.
    #[serde(default)]
    pub v: u16,
    /// The payload (base64 in JSON).
    #[serde(default, with = "crate::serde_util::b64")]
    pub bytes: Vec<u8>,
}

impl BackendBlob {
    /// A blob with version `v`.
    pub fn new(v: u16, bytes: Vec<u8>) -> Self {
        Self { v, bytes }
    }
}

impl fmt::Debug for BackendBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BackendBlob {{ v: {}, len: {} }}",
            self.v,
            self.bytes.len()
        )
    }
}

/// Kinds of human-facing identifiers (for display, search and "start chat").
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub enum IdentifierKind {
    /// A phone number in E.164 form.
    Phone,
    /// A username.
    Username,
    /// An e-mail address.
    Email,
    /// A network id shown to users (Threema ID).
    NetworkId,
}

/// A human-facing identifier of a contact (phone, username, …). Never a required field.
/// `Debug` hides the value.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Identifier {
    /// What the value is.
    pub kind: IdentifierKind,
    /// The value as the user sees it. Not log-safe.
    pub value: String,
}

impl fmt::Debug for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Identifier {{ kind: {:?}, value: [redacted] }}",
            self.kind
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("valid user id")
    }

    #[test]
    fn user_id_grammar() {
        assert_eq!(uid("aci:abc").scheme(), "aci");
        assert_eq!(uid("e164:+491234").value(), "+491234");
        assert_eq!(uid("lid:1:2@lid").value(), "1:2@lid");
        assert_eq!(UserId::parse(""), Err(IdError::Empty));
        assert_eq!(UserId::parse("ECHOECHO"), Err(IdError::BadScheme));
        assert_eq!(UserId::parse("ACI:x"), Err(IdError::BadScheme));
        assert_eq!(UserId::parse("1ci:x"), Err(IdError::BadScheme));
        assert_eq!(UserId::parse("aci:"), Err(IdError::Empty));
        assert_eq!(UserId::parse("aci:a b"), Err(IdError::BadChar));
        assert_eq!(UserId::parse("aci:a\nb"), Err(IdError::BadChar));
        assert_eq!(
            UserId::parse(&format!("aci:{}", "x".repeat(MAX_ID_LEN))),
            Err(IdError::TooLong)
        );
        assert_eq!(
            UserId::parse(&format!("{}:x", "a".repeat(MAX_SCHEME_LEN + 1))),
            Err(IdError::BadScheme)
        );
        assert_eq!(uid("th-id:X").as_str(), "th-id:X");
        let u = UserId::from_parts("e164", "+491234").expect("valid parts");
        assert_eq!((u.scheme(), u.value()), ("e164", "+491234"));
        assert_eq!(UserId::from_parts("a:b", "c"), Err(IdError::BadScheme));
        assert_eq!(UserId::from_parts("", "c"), Err(IdError::BadScheme));
        assert_eq!(UserId::from_parts("aci", ""), Err(IdError::Empty));
    }

    #[test]
    fn conv_key_grammar() {
        let k = ConvKey::parse("direct:aci:abc").expect("valid");
        assert_eq!(k, ConvKey::Direct(uid("aci:abc")));
        assert_eq!(k.encode(), "direct:aci:abc");
        let g = ConvKey::parse("group:gv2:AbC-_").expect("valid");
        assert_eq!(g, ConvKey::Group("gv2:AbC-_".into()));
        assert_eq!(g.kind(), crate::conversation::ConvKind::Group);
        assert_eq!(ConvKey::parse("room:x:y"), Err(IdError::UnknownPrefix));
        assert_eq!(ConvKey::parse("group"), Err(IdError::UnknownPrefix));
        assert_eq!(
            ConvKey::parse("group:nogroupscheme"),
            Err(IdError::BadScheme)
        );
        assert_eq!(
            ConvKey::Group("bad".into()).validate(),
            Err(IdError::BadScheme)
        );
    }

    #[test]
    fn net_msg_id_grammar_and_escaping() {
        let own = NetMsgId::own("1700000000000");
        assert_eq!(own.encode(), "m1:1700000000000");
        assert_eq!(own.author_column(), "");
        let tricky = NetMsgId::remote(uid("lid:9@lid"), "a@b%40c%");
        let s = tricky.encode();
        assert_eq!(s, "m1:a%40b%2540c%25@lid:9@lid");
        assert_eq!(NetMsgId::parse(&s), Ok(tricky));
        assert_eq!(NetMsgId::parse("m1:"), Err(IdError::Empty));
        assert_eq!(NetMsgId::parse("m2:1"), Err(IdError::UnknownPrefix));
        assert_eq!(NetMsgId::parse("m1:a%2"), Err(IdError::BadChar));
        assert_eq!(NetMsgId::parse("m1:a%41"), Err(IdError::BadChar));
        assert_eq!(NetMsgId::parse("m1:1@nope"), Err(IdError::BadScheme));
    }

    #[test]
    fn target_matching() {
        let a = uid("aci:a");
        let id = NetMsgId::remote(a.clone(), "5");
        let t = MsgTarget::to_id(None, &id);
        assert!(t.matches(&id));
        assert_eq!(t.net_id(), Some(id.clone()));
        assert!(!t.matches(&NetMsgId::own("5")));
        assert!(!t.matches(&NetMsgId::remote(uid("aci:b"), "5")));
        let unknown = MsgTarget {
            conv: None,
            author: TargetAuthor::Unknown,
            local: "6".into(),
            alternates: vec!["5".into()],
        };
        assert!(unknown.matches(&id));
        assert_eq!(unknown.net_id(), None);
        let own = MsgTarget::to_id(None, &NetMsgId::own("7"));
        assert!(own.matches(&NetMsgId::own("7")));
        assert!(!own.matches(&NetMsgId::remote(a, "7")));
    }

    #[test]
    fn scope_and_kind_codes() {
        for s in [MsgIdScope::AccountAuthor, MsgIdScope::Conversation] {
            assert_eq!(MsgIdScope::from_db_code(s.db_code()), Some(s));
        }
        assert_eq!(MsgIdScope::from_db_code(2), None);
        for k in NetworkKind::ALL {
            assert_eq!(k.as_str().parse::<NetworkKind>(), Ok(k));
        }
    }

    #[test]
    fn account_uuid_display_roundtrip() {
        let u = AccountUuid::new_random();
        assert_eq!(u.to_string().parse::<AccountUuid>().ok(), Some(u));
        assert_eq!(u.short().len(), 8);
        assert!(u.to_string().starts_with(&u.short()));
    }

    #[test]
    fn debug_of_ids_is_log_safe() {
        let secret = "5f3c2a1e-9b8d-4c7e-a6f5-0123456789ab";
        let u = uid(&format!("aci:{secret}"));
        let dbg = format!(
            "{u:?} {:?} {:?}",
            ConvKey::Direct(u.clone()),
            NetMsgId::remote(u.clone(), "1")
        );
        assert!(!dbg.contains(secret), "{dbg}");
        assert!(dbg.contains("aci:…89ab"), "{dbg}");
        let blob = BackendBlob::new(1, b"secret payload".to_vec());
        assert_eq!(format!("{blob:?}"), "BackendBlob { v: 1, len: 14 }");
        let ident = Identifier {
            kind: IdentifierKind::Phone,
            value: "+4915112345678".into(),
        };
        assert!(!format!("{ident:?}").contains("1234"));
    }
}
