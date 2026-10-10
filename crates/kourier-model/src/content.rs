//! Messages and their content (architecture §3.5).

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use crate::call::CallLine;
use crate::ids::{
    AttachmentId, BackendBlob, ConversationId, Identifier, MessageId, MsgTarget, NetMsgId, UserId,
};
use crate::log::Shape;
use crate::state::{OutgoingState, ReadState};
use crate::text::Text;

/// Who sent a message and from which device.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Origin {
    /// Another user. `device` lets a message clear that device's typing indicator.
    Remote {
        /// The author.
        author: UserId,
        /// The author's device, when the network says.
        #[serde(default)]
        device: Option<u32>,
    },
    /// Sent from this Kourier account.
    OwnThisDevice,
    /// Sent by our account from another device (sync transcript, echo).
    OwnOtherDevice {
        /// Our other device, when known.
        #[serde(default)]
        device: Option<u32>,
    },
}

impl Origin {
    /// The author for [`NetMsgId::author`]: `None` for own messages.
    pub fn author(&self) -> Option<&UserId> {
        match self {
            Self::Remote { author, .. } => Some(author),
            Self::OwnThisDevice | Self::OwnOtherDevice { .. } => None,
        }
    }

    /// Whether our account sent it.
    pub fn is_own(&self) -> bool {
        !matches!(self, Self::Remote { .. })
    }

    /// The sending device, when known.
    pub fn device(&self) -> Option<u32> {
        match self {
            Self::Remote { device, .. } | Self::OwnOtherDevice { device } => *device,
            Self::OwnThisDevice => None,
        }
    }
}

/// A message's content. `Debug` is log-safe (texts, names and summaries are redacted).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Body {
    /// Text only.
    Text(Text),
    /// One or more attachments with an optional caption.
    Media {
        /// The attachments, in order.
        attachments: Vec<Attachment>,
        /// Caption for the whole message.
        #[serde(default)]
        caption: Option<Text>,
    },
    /// A sticker.
    Sticker(Box<Attachment>),
    /// A shared contact.
    Contact(ContactCard),
    /// A location (reserved: Threema, WhatsApp).
    Location(Location),
    /// A poll (reserved).
    Poll(Poll),
    /// A system notice in the timeline.
    System(SystemEvent),
    /// A stand-in for content that is not (or no longer) available.
    Placeholder(Placeholder),
    /// Content Kourier does not understand; the raw payload is kept in `Message.raw`.
    Unsupported {
        /// Short, log-safe kind name ("payment", "story-reply", …).
        kind_hint: String,
        /// User-facing summary.
        #[serde(default)]
        summary: Option<String>,
    },
}

impl Body {
    /// Plain text body.
    pub fn text(body: impl Into<String>) -> Self {
        Self::Text(Text::plain(body))
    }

    /// The variant name (`messages.body_kind`).
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "Text",
            Self::Media { .. } => "Media",
            Self::Sticker(_) => "Sticker",
            Self::Contact(_) => "Contact",
            Self::Location(_) => "Location",
            Self::Poll(_) => "Poll",
            Self::System(_) => "System",
            Self::Placeholder(_) => "Placeholder",
            Self::Unsupported { .. } => "Unsupported",
        }
    }

    /// The call line, when the body is one.
    pub fn call_line(&self) -> Option<&CallLine> {
        match self {
            Self::System(SystemEvent::Call(line)) => Some(line),
            _ => None,
        }
    }

    /// Every [`Text`] in the body (text, caption, attachment captions).
    pub fn texts_mut(&mut self) -> Vec<&mut Text> {
        match self {
            Self::Text(t) => vec![t],
            Self::Media {
                attachments,
                caption,
            } => caption
                .iter_mut()
                .chain(attachments.iter_mut().filter_map(|a| a.caption.as_mut()))
                .collect(),
            Self::Sticker(a) => a.caption.iter_mut().collect(),
            _ => Vec::new(),
        }
    }

    /// Drops invalid ranges in every text of the body (§3.6); returns how many were dropped.
    pub fn sanitize_ranges(&mut self) -> usize {
        self.texts_mut().into_iter().map(Text::sanitize).sum()
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(t) => f.debug_tuple("Text").field(t).finish(),
            Self::Media {
                attachments,
                caption,
            } => f
                .debug_struct("Media")
                .field("attachments", attachments)
                .field("caption", caption)
                .finish(),
            Self::Sticker(a) => f.debug_tuple("Sticker").field(a).finish(),
            Self::Contact(c) => f.debug_tuple("Contact").field(c).finish(),
            Self::Location(l) => f.debug_tuple("Location").field(l).finish(),
            Self::Poll(p) => f.debug_tuple("Poll").field(p).finish(),
            Self::System(e) => f.debug_tuple("System").field(e).finish(),
            Self::Placeholder(p) => f.debug_tuple("Placeholder").field(p).finish(),
            Self::Unsupported { kind_hint, summary } => f
                .debug_struct("Unsupported")
                .field("kind_hint", kind_hint)
                .field("summary", &Shape(summary))
                .finish(),
        }
    }
}

/// Kind of an attachment.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum AttachmentKind {
    /// Still image.
    Image,
    /// Video.
    Video,
    /// Audio file.
    Audio,
    /// Voice note.
    Voice,
    /// Animated GIF (or a looping video shown as one).
    Gif,
    /// Any other file, and a picture, GIF or video the user sends as the file it is ("send as
    /// file"): its `mime` is then the true type of the content (`image/jpeg`), and its bytes
    /// are the user's. A backend sends a `File` as it is, whatever its type: it never
    /// re-encodes, scales or strips one, and gives it no thumbnail, dimensions or flags of a
    /// picture.
    #[default]
    File,
}

/// Download state of an attachment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttachmentState {
    /// Only the remote pointer is known.
    #[default]
    Remote,
    /// Downloading.
    Downloading {
        /// Bytes done.
        done: u64,
        /// Total bytes.
        total: u64,
    },
    /// Stored locally.
    Local,
    /// The download failed.
    Failed {
        /// Whether trying again may help.
        retryable: bool,
    },
    /// The network no longer has it.
    Expired,
}

/// An attachment of a message. `Debug` hides the file name, blurhash and waveform.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Attachment {
    /// Core rowid, once stored.
    pub id: Option<AttachmentId>,
    /// Kind.
    pub kind: AttachmentKind,
    /// MIME type.
    pub mime: String,
    /// Size in bytes (plaintext).
    pub size: u64,
    /// File name. Not log-safe.
    pub file_name: Option<String>,
    /// Width and height in pixels.
    pub dims: Option<(u32, u32)>,
    /// Duration of audio/video.
    pub duration_ms: Option<u32>,
    /// Blurhash placeholder.
    pub blurhash: Option<String>,
    /// Voice-note waveform.
    #[serde(with = "crate::serde_util::b64_opt")]
    pub waveform: Option<Vec<u8>>,
    /// Per-attachment caption.
    pub caption: Option<Text>,
    /// Download state.
    pub state: AttachmentState,
    /// Network pointer (Signal `AttachmentPointer` bytes, WhatsApp directPath + mediaKey, …).
    pub remote: Option<BackendBlob>,
    /// The local encrypted file (core-owned).
    pub local: Option<BlobRef>,
    /// The local encrypted thumbnail.
    pub thumb: Option<BlobRef>,
    /// This attachment is the rest of the message's text, not a file for the user: the text or
    /// caption beside it is only the beginning (Signal sends a body above 2 KiB trimmed, with
    /// the whole text as a `text/x-signal-plain` attachment). A backend that could not fetch
    /// the whole text before it emitted the message sets this; the core downloads the
    /// attachment whatever the user's download settings say and, once it is here, replaces the
    /// text with the file's content (UTF-8) and drops the attachment. The UI does not show it
    /// as a file.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub long_text: bool,
    /// Names the network has for this attachment, opaque to the core (Signal: its client
    /// UUID and its digest). A backend sets them on the attachments it emits when the network
    /// can later refer to one attachment of a message; `BackendEvent::AttachmentDeleted` then
    /// names the attachment by any of them. For the attachments of a message this device
    /// sends, the core fills them from `Network::attachment_net_ids` when it queues the
    /// message, and the request carries them back to the backend. A delete of the
    /// attachment made here hands them to the backend (`Command::DeleteAttachment`). Kept
    /// with the message; never shown.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub net_ids: Vec<String>,
}

crate::redacting_debug!(Attachment {
    id, kind, mime, size, dims, duration_ms, caption, state, remote, local, thumb, long_text
} redact { file_name, blurhash, waveform, net_ids });

/// What a backend learned while downloading an attachment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentMeta {
    /// Plaintext size written.
    pub size: u64,
    /// MIME type, when the network or the content says.
    pub mime: Option<String>,
    /// Width and height, when known.
    pub dims: Option<(u32, u32)>,
    /// Duration, when known.
    pub duration_ms: Option<u32>,
}

/// A 32-byte file key, zeroized on drop. `Debug` is redacted; serde is base64.
#[derive(Clone, PartialEq, Eq)]
pub struct BlobKey(Zeroizing<[u8; 32]>);

impl std::hash::Hash for BlobKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0[..].hash(state);
    }
}

impl BlobKey {
    /// Wraps key bytes.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The key bytes. Never log them.
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for BlobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BlobKey([redacted])")
    }
}

impl Serialize for BlobKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::serde_util::encode(&self.0[..]))
    }
}

impl<'de> Deserialize<'de> for BlobKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = Zeroizing::new(String::deserialize(d)?);
        crate::serde_util::decode_fixed::<32>(&s)
            .map(Self::new)
            .ok_or_else(|| D::Error::custom("blob key must be 32 bytes of base64"))
    }
}

/// A `kourier-blob` file and its key (§5.8). `path` is relative to the account's blob
/// directory (`AccountContext::blob_dir`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobRef {
    /// File path relative to the account's `blobs/` directory.
    pub path: PathBuf,
    /// The file key.
    pub key: BlobKey,
}

/// A quoted reply.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplyTo {
    /// The quoted message.
    pub target: MsgTarget,
    /// The quoted author as the quote states it (`None` = own or unknown).
    #[serde(default)]
    pub author: Option<UserId>,
    /// What the quote showed when sent, for display when the target is missing.
    #[serde(default)]
    pub snapshot: Option<QuoteSnapshot>,
}

/// The sender's copy of a quoted message. `Debug` hides the file name.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct QuoteSnapshot {
    /// Quoted text.
    pub text: Option<Text>,
    /// Kind of the first quoted attachment.
    pub attachment: Option<AttachmentKind>,
    /// Its file name. Not log-safe.
    pub file_name: Option<String>,
    /// Its MIME type, when known (Signal's quote names it; the core fills it for own replies).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// The quoted attachment's encrypted thumbnail in the account's blob directory. Set by the
    /// core only in the request of an own reply (`OutgoingKind::Message.reply_to`), for
    /// networks that send a thumbnail with the quote; never stored with a message and never
    /// set by a backend. (Boxed: it is rare, and a quote is part of every message event.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb: Option<Box<BlobRef>>,
}

crate::redacting_debug!(QuoteSnapshot { text, attachment, mime, thumb } redact { file_name });

/// A shared contact card. `Debug` hides the names.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactCard {
    /// Display name.
    pub name: Option<String>,
    /// Organization.
    pub organization: Option<String>,
    /// Phone numbers, e-mail addresses, …
    pub identifiers: Vec<Identifier>,
    /// Avatar file.
    pub avatar: Option<BlobRef>,
}

crate::redacting_debug!(ContactCard { identifiers, avatar } redact { name, organization });

/// A location, in 1e-7 degrees (integers keep the model `Eq` and exact). `Debug` hides
/// everything.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Location {
    /// Latitude × 10⁷.
    pub lat_e7: i32,
    /// Longitude × 10⁷.
    pub lon_e7: i32,
    /// Place name.
    pub name: Option<String>,
    /// Address.
    pub address: Option<String>,
}

crate::redacting_debug!(Location {} redact { lat_e7, lon_e7, name, address });

/// A poll (reserved). `Debug` hides the question and options.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Poll {
    /// The question.
    pub question: String,
    /// The options.
    pub options: Vec<String>,
    /// Several options may be chosen.
    pub multiple: bool,
}

crate::redacting_debug!(Poll { multiple } redact { question, options });

/// A system notice (§3.5). `by: None` = unknown or own. `Debug` hides the title.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SystemEvent {
    /// Members were added.
    MembersAdded {
        /// Who added them.
        #[serde(default)]
        by: Option<UserId>,
        /// The new members.
        users: Vec<UserId>,
    },
    /// Members were removed.
    MembersRemoved {
        /// Who removed them.
        #[serde(default)]
        by: Option<UserId>,
        /// The removed members.
        users: Vec<UserId>,
    },
    /// A member left.
    MemberLeft {
        /// Who left.
        user: UserId,
    },
    /// The title changed.
    TitleChanged {
        /// Who changed it.
        #[serde(default)]
        by: Option<UserId>,
        /// The new title.
        title: String,
    },
    /// The avatar changed.
    AvatarChanged {
        /// Who changed it.
        #[serde(default)]
        by: Option<UserId>,
    },
    /// The description changed.
    DescriptionChanged {
        /// Who changed it.
        #[serde(default)]
        by: Option<UserId>,
    },
    /// The disappearing-message timer changed.
    TimerChanged {
        /// Who changed it.
        #[serde(default)]
        by: Option<UserId>,
        /// The new timer; `None` = off.
        #[serde(default)]
        timer: Option<Duration>,
    },
    /// A contact's identity key (safety number) changed.
    IdentityKeyChanged {
        /// Whose key.
        user: UserId,
    },
    /// The secure session with a contact was reset.
    SessionRefreshed {
        /// With whom.
        user: UserId,
    },
    /// The group was created.
    GroupCreated {
        /// By whom.
        #[serde(default)]
        by: Option<UserId>,
    },
    /// We accepted the message request.
    MessageRequestAccepted,
    /// A call, as a line that changes with the call's state ([`crate::call`]). Its row is
    /// found by its network-local id (`call:<id>`, `gcall:<id>`) and has no author. (Reads
    /// the `MissedCall` notice it replaced as a missed line.)
    #[serde(alias = "MissedCall")]
    Call(CallLine),
    /// We joined.
    Joined,
    /// A contact changed their number.
    NumberChanged {
        /// Who.
        user: UserId,
    },
}

impl fmt::Debug for SystemEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let by = |f: &mut fmt::Formatter<'_>, name: &str, by: &Option<UserId>| {
            f.debug_struct(name).field("by", by).finish()
        };
        let user = |f: &mut fmt::Formatter<'_>, name: &str, user: &UserId| {
            f.debug_struct(name).field("user", user).finish()
        };
        match self {
            Self::MembersAdded { by, users } => f
                .debug_struct("MembersAdded")
                .field("by", by)
                .field("users", users)
                .finish(),
            Self::MembersRemoved { by, users } => f
                .debug_struct("MembersRemoved")
                .field("by", by)
                .field("users", users)
                .finish(),
            Self::MemberLeft { user: u } => user(f, "MemberLeft", u),
            Self::TitleChanged { by, title } => f
                .debug_struct("TitleChanged")
                .field("by", by)
                .field("title", &Shape(title))
                .finish(),
            Self::AvatarChanged { by: b } => by(f, "AvatarChanged", b),
            Self::DescriptionChanged { by: b } => by(f, "DescriptionChanged", b),
            Self::TimerChanged { by, timer } => f
                .debug_struct("TimerChanged")
                .field("by", by)
                .field("timer", timer)
                .finish(),
            Self::IdentityKeyChanged { user: u } => user(f, "IdentityKeyChanged", u),
            Self::SessionRefreshed { user: u } => user(f, "SessionRefreshed", u),
            Self::GroupCreated { by: b } => by(f, "GroupCreated", b),
            Self::MessageRequestAccepted => f.write_str("MessageRequestAccepted"),
            Self::Call(line) => f.debug_tuple("Call").field(line).finish(),
            Self::Joined => f.write_str("Joined"),
            Self::NumberChanged { user: u } => user(f, "NumberChanged", u),
        }
    }
}

/// Stand-ins for unavailable content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Placeholder {
    /// Could not be decrypted; `resendable` = the sender was asked to resend it.
    Undecryptable {
        /// Whether a resend may still replace it.
        #[serde(default)]
        resendable: bool,
    },
    /// A view-once message that was viewed.
    ViewOnceOpened,
    /// The content expired.
    Expired,
}

/// How a message was deleted (tombstone, §3.5.2).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Deletion {
    /// Deleted for everyone by its author (`None` = own) or an admin.
    ForEveryone {
        /// Who deleted it.
        #[serde(default)]
        by: Option<UserId>,
    },
    /// Deleted on this account only.
    ForMe,
}

/// When a disappearing-message timer starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DisappearKind {
    /// After the message is read.
    AfterRead,
    /// After the message is sent.
    AfterSend,
}

/// A message's disappearing timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Expiry {
    /// When the timer starts.
    pub kind: DisappearKind,
    /// The timer.
    pub timer: Duration,
    /// When it started (ms), once it has.
    #[serde(default)]
    pub started_at: Option<i64>,
}

impl Expiry {
    /// When the message expires (ms), once the timer has started.
    pub fn expires_at(&self) -> Option<i64> {
        let ms = i64::try_from(self.timer.as_millis()).unwrap_or(i64::MAX);
        self.started_at.map(|s| s.saturating_add(ms))
    }
}

/// A message row (§3.5). Timestamps are ms since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Core rowid.
    pub id: MessageId,
    /// Its conversation.
    pub conversation: ConversationId,
    /// Network id; `net_id.author` is the author (`None` = own account).
    pub net_id: NetMsgId,
    /// Who sent it, from which device.
    pub origin: Origin,
    /// Content (latest revision).
    pub body: Body,
    /// Quoted message.
    #[serde(default)]
    pub reply_to: Option<ReplyTo>,
    /// The sender's timestamp.
    pub sent_at: i64,
    /// Server timestamp.
    #[serde(default)]
    pub server_at: Option<i64>,
    /// Local wall clock when the backend's stage 1 stored it.
    pub received_at: i64,
    /// The backend's stage-1 arrival counter.
    #[serde(default)]
    pub arrival: Option<u64>,
    /// Core-assigned timeline order (§3.9).
    pub sort_key: i64,
    /// Edit revision (0 = original).
    #[serde(default)]
    pub revision: u16,
    /// When the latest edit was made.
    #[serde(default)]
    pub edited_at: Option<i64>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: Option<Deletion>,
    /// Own messages: send state.
    #[serde(default)]
    pub out_state: Option<OutgoingState>,
    /// Incoming messages: read state.
    #[serde(default)]
    pub read_state: Option<ReadState>,
    /// Disappearing timer.
    #[serde(default)]
    pub expiry: Option<Expiry>,
    /// View-once.
    #[serde(default)]
    pub view_once: bool,
    /// Imported history (never notifies).
    #[serde(default)]
    pub history: bool,
    /// The broadcast send this per-member copy belongs to.
    #[serde(default)]
    pub linked: Option<MessageId>,
    /// Backend per-row metadata.
    #[serde(default)]
    pub meta: Option<BackendBlob>,
    /// Raw payload for unsupported content.
    #[serde(default)]
    pub raw: Option<BackendBlob>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{RangeKind, TextRange};

    #[test]
    fn origin_helpers() {
        let a = UserId::parse("aci:a").expect("valid");
        let r = Origin::Remote {
            author: a.clone(),
            device: Some(2),
        };
        assert_eq!(r.author(), Some(&a));
        assert!(!r.is_own());
        assert_eq!(r.device(), Some(2));
        assert!(Origin::OwnThisDevice.is_own() && Origin::OwnThisDevice.author().is_none());
        assert_eq!(Origin::OwnOtherDevice { device: Some(3) }.device(), Some(3));
    }

    #[test]
    fn sanitize_covers_captions() {
        let bad = || TextRange {
            start: 0,
            len: 50,
            kind: RangeKind::Bold,
        };
        let text = |s: &str| Text {
            body: s.into(),
            ranges: vec![bad()],
        };
        let mut body = Body::Media {
            attachments: vec![Attachment {
                caption: Some(text("a")),
                ..Attachment::default()
            }],
            caption: Some(text("b")),
        };
        assert_eq!(body.sanitize_ranges(), 2);
        assert_eq!(body.sanitize_ranges(), 0);
        assert_eq!(body.kind_name(), "Media");
    }

    #[test]
    fn blob_key_is_redacted_and_roundtrips() {
        let k = BlobKey::new([7; 32]);
        assert_eq!(format!("{k:?}"), "BlobKey([redacted])");
        let json = serde_json::to_string(&k).expect("serialize");
        assert_eq!(serde_json::from_str::<BlobKey>(&json).ok(), Some(k));
        assert!(serde_json::from_str::<BlobKey>("\"AAAA\"").is_err());
    }

    #[test]
    fn debug_of_content_is_log_safe() {
        let secret = |s: &str| format!("SECRET-{s}");
        let attachment = Attachment {
            file_name: Some(secret("file.pdf")),
            blurhash: Some(secret("hash")),
            caption: Some(Text::plain(secret("caption"))),
            ..Attachment::default()
        };
        let bodies = [
            Body::text(secret("hello")),
            Body::Media {
                attachments: vec![attachment.clone()],
                caption: Some(Text::plain(secret("cap"))),
            },
            Body::Sticker(Box::new(attachment)),
            Body::Contact(ContactCard {
                name: Some(secret("name")),
                organization: Some(secret("org")),
                identifiers: vec![Identifier {
                    kind: crate::ids::IdentifierKind::Phone,
                    value: secret("+49"),
                }],
                avatar: None,
            }),
            Body::Location(Location {
                lat_e7: 525_200_000,
                lon_e7: 134_000_000,
                name: Some(secret("place")),
                address: Some(secret("street")),
            }),
            Body::Poll(Poll {
                question: secret("q"),
                options: vec![secret("a")],
                multiple: true,
            }),
            Body::System(SystemEvent::TitleChanged {
                by: None,
                title: secret("title"),
            }),
            Body::Unsupported {
                kind_hint: "payment".into(),
                summary: Some(secret("5 MOB")),
            },
        ];
        for b in &bodies {
            let s = format!("{b:?} {b:#?}");
            assert!(!s.contains("SECRET") && !s.contains("525"), "{s}");
        }
        let q = QuoteSnapshot {
            file_name: Some(secret("f")),
            ..QuoteSnapshot::default()
        };
        assert!(!format!("{q:?}").contains("SECRET"));
    }

    #[test]
    fn expiry_time() {
        let e = Expiry {
            kind: DisappearKind::AfterRead,
            timer: Duration::from_secs(5),
            started_at: Some(1_000),
        };
        assert_eq!(e.expires_at(), Some(6_000));
        assert_eq!(
            Expiry {
                started_at: None,
                ..e
            }
            .expires_at(),
            None
        );
    }
}
