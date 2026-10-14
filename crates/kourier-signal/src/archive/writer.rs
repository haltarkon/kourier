//! The writer of a transfer archive, for tests (`cfg(test)` or the `testing` feature): frames
//! in, an encrypted archive file out, the way the phone writes one. Later steps use it to feed
//! the mock server and to build archives for the import's tests.
//!
//! The file is `IV ‖ AES-256-CBC(gzip(frames) ‖ zero padding) ‖ HMAC-SHA256(IV ‖ ciphertext)`.
//! The encryption is the attachment encryptor's (`crate::attachment::Encryptor`: the same
//! IV-first, MAC-last container), which also pads the plaintext with zeros to a bucket size;
//! libsignal's reader and ours both leave bytes after the end of the gzip stream alone. The
//! whole compressed stream is held in memory: this is for small test archives (a gzip bomb of
//! gigabytes is a few hundred KiB compressed).

use std::io::{self, Write as _};
use std::path::Path;

use flate2::Compression;
use flate2::write::GzEncoder;
use libsignal_core::Aci;
use prost::Message as _;

use super::{ArchiveInfo, ArchiveKey, Frame, SUPPORTED_VERSION};
use crate::attachment::{AttachmentKeys, Encryptor};
use crate::proto::backup;

/// Builds one archive: the `BackupInfo` first, then frames, then [`Self::write`].
pub(crate) struct ArchiveWriter {
    gzip: GzEncoder<Vec<u8>>,
}

impl ArchiveWriter {
    /// A writer whose first frame is `info` (written as it is: a test can give a version this
    /// reader does not know).
    pub(crate) fn new(info: &ArchiveInfo) -> Self {
        let mut writer = Self {
            gzip: GzEncoder::new(Vec::new(), Compression::fast()),
        };
        writer.raw_frame(&info.to_proto().encode_to_vec());
        writer
    }

    /// Adds a frame: its varint length, then the protobuf message.
    pub(crate) fn frame(&mut self, frame: &Frame) -> &mut Self {
        self.raw_frame(&frame.to_proto().encode_to_vec())
    }

    /// Adds a frame whose protobuf bytes are given (a test of a malformed or future frame).
    pub(crate) fn raw_frame(&mut self, payload: &[u8]) -> &mut Self {
        self.raw(&varint(payload.len() as u64)).raw(payload)
    }

    /// Adds bytes to the stream as they are, with no length prefix.
    pub(crate) fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.gzip
            .write_all(bytes)
            .expect("writing to a Vec cannot fail");
        self
    }

    /// The gzip stream of everything added (not encrypted).
    pub(crate) fn compressed(self) -> Vec<u8> {
        self.gzip.finish().expect("writing to a Vec cannot fail")
    }

    /// Encrypts the archive with a random IV and writes it to `path`.
    pub(crate) fn write(self, path: &Path, key: &ArchiveKey) -> io::Result<()> {
        self.write_with_iv(path, key, rand::random())
    }

    /// Encrypts the archive with the given IV and writes it to `path`.
    pub(crate) fn write_with_iv(
        self,
        path: &Path,
        key: &ArchiveKey,
        iv: [u8; 16],
    ) -> io::Result<()> {
        seal(path, key, iv, &self.compressed())
    }
}

/// Writes `compressed` (the bytes that should be a gzip stream) as an archive file at `path`:
/// the cryptography without the compression, so a test can seal bytes that are not valid gzip.
pub(crate) fn seal(
    path: &Path,
    key: &ArchiveKey,
    iv: [u8; 16],
    compressed: &[u8],
) -> io::Result<()> {
    std::fs::write(path, seal_bytes(key, iv, compressed)?)
}

/// The bytes of [`seal`].
pub(crate) fn seal_bytes(key: &ArchiveKey, iv: [u8; 16], compressed: &[u8]) -> io::Result<Vec<u8>> {
    let other = |error: crate::attachment::AttachmentError| io::Error::other(error.to_string());
    let keys = AttachmentKeys::from_slice(&key.aes_then_hmac()[..]).map_err(other)?;
    let mut encryptor = Encryptor::new(&keys, iv, compressed.len() as u64, false).map_err(other)?;
    let mut out = Vec::new();
    encryptor.update(compressed, &mut out).map_err(other)?;
    encryptor.finish(&mut out).map_err(other)?;
    Ok(out)
}

/// Like [`seal_bytes`] with no zero padding after the compressed bytes: `IV ‖ AES-256-CBC(
/// compressed) ‖ HMAC`, the shortest file, with the one-shot primitives. A test of the end of the
/// stream (a cut gzip stream is not followed by padding) and of the padding itself.
#[cfg(test)]
pub(crate) fn seal_unpadded_bytes(key: &ArchiveKey, iv: [u8; 16], compressed: &[u8]) -> Vec<u8> {
    let keys = key.message_backup_key();
    let mut out = iv.to_vec();
    out.extend(
        signal_crypto::aes_256_cbc_encrypt(compressed, &keys.aes_key, &iv)
            .expect("a 32-byte key and a 16-byte IV encrypt"),
    );
    let mac = mac(key, &out);
    out.extend_from_slice(&mac);
    out
}

/// The archive's MAC over `iv_and_ciphertext` (a test that changes ciphertext and keeps the MAC
/// valid, to reach the checks behind it).
#[cfg(test)]
pub(crate) fn mac(key: &ArchiveKey, iv_and_ciphertext: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac as _};
    let mut mac =
        <Hmac<sha2::Sha256> as hmac::KeyInit>::new_from_slice(&key.message_backup_key().hmac_key)
            .expect("HMAC takes any key length");
    mac.update(iv_and_ciphertext);
    mac.finalize().into_bytes().into()
}

/// The protobuf varint of `value`.
pub(crate) fn varint(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    prost::encoding::encode_varint(value, &mut out);
    out
}

/// A small, valid backup, the same in every call: the account, the note-to-self and one
/// contact, a chat with the contact and four chat items (outgoing, incoming, an incoming one
/// with a reaction, and an incoming one that is read). Synthetic data; the names are made up.
pub(crate) fn sample_info() -> ArchiveInfo {
    ArchiveInfo {
        version: SUPPORTED_VERSION,
        backup_time_ms: 1_700_000_000_000,
        current_app_version: "8.30.2".to_owned(),
        first_app_version: "7.0.0".to_owned(),
        media_root_backup_key: Some(zeroize::Zeroizing::new([7; 32])),
    }
}

/// The frames of the sample backup after its `BackupInfo`.
pub(crate) fn sample_frames() -> Vec<Frame> {
    use backup::chat_item::{DirectionalDetails, IncomingMessageDetails, Item};
    use backup::recipient::Destination;

    let text_item = |chat_id, author_id, date_sent, body: &str, details, reactions| {
        Frame::ChatItem(Box::new(backup::ChatItem {
            chat_id,
            author_id,
            date_sent,
            directional_details: Some(details),
            item: Some(Item::StandardMessage(backup::StandardMessage {
                text: Some(backup::Text {
                    body: body.to_owned(),
                    body_ranges: Vec::new(),
                }),
                reactions,
                ..Default::default()
            })),
            ..Default::default()
        }))
    };
    let incoming = |date_received, read| {
        DirectionalDetails::Incoming(IncomingMessageDetails {
            date_received,
            read,
            sealed_sender: true,
            ..Default::default()
        })
    };
    let outgoing = DirectionalDetails::Outgoing(backup::chat_item::OutgoingMessageDetails {
        send_status: vec![backup::SendStatus {
            recipient_id: 2,
            timestamp: 3001,
            delivery_status: Some(backup::send_status::DeliveryStatus::Sent(
                backup::send_status::Sent {
                    sealed_sender: true,
                },
            )),
        }],
        ..Default::default()
    });

    vec![
        Frame::Account(Box::new(backup::AccountData {
            profile_key: vec![1; 32],
            given_name: "Test".to_owned(),
            family_name: "Account".to_owned(),
            account_settings: Some(backup::account_data::AccountSettings {
                phone_number_sharing_mode: 2,
                default_sent_media_quality: 1,
                app_theme: 1,
                calls_use_less_data_setting: 2,
                unread_badge_type: 1,
                ..Default::default()
            }),
            android_specific_settings: Some(backup::account_data::AndroidSpecificSettings {
                use_system_emoji: true,
                screenshot_security: true,
                navigation_bar_size: 1,
            }),
            ..Default::default()
        })),
        Frame::Recipient(backup::Recipient {
            id: 1,
            destination: Some(Destination::Self_(backup::Self_::default())),
        }),
        Frame::Recipient(backup::Recipient {
            id: 2,
            destination: Some(Destination::Contact(backup::Contact {
                aci: Some(vec![0x5f; 16]),
                profile_key: Some(vec![2; 32]),
                profile_sharing: true,
                profile_given_name: Some("Sample".to_owned()),
                profile_family_name: Some("Contact".to_owned()),
                registration: Some(backup::contact::Registration::Registered(
                    backup::contact::Registered {},
                )),
                ..Default::default()
            })),
        }),
        Frame::Chat(backup::Chat {
            id: 1,
            recipient_id: 2,
            ..Default::default()
        }),
        text_item(1, 1, 3000, "first, from the phone", outgoing, Vec::new()),
        text_item(
            1,
            2,
            3100,
            "second, from the contact",
            incoming(3101, true),
            Vec::new(),
        ),
        text_item(
            1,
            2,
            3200,
            "third, with a reaction",
            incoming(3201, false),
            vec![backup::Reaction {
                emoji: "\u{1f44d}".to_owned(),
                author_id: 1,
                sent_timestamp: 3300,
                sort_order: 1,
            }],
        ),
    ]
}

/// The sample backup as a file at `path`, encrypted for a device that was linked with
/// `ephemeral_key` as account `own_aci`.
pub(crate) fn write_sample(path: &Path, ephemeral_key: &[u8; 32], own_aci: Aci) -> io::Result<()> {
    let key = ArchiveKey::for_transfer(ephemeral_key, own_aci);
    let mut writer = ArchiveWriter::new(&sample_info());
    for frame in sample_frames() {
        writer.frame(&frame);
    }
    writer.write(path, &key)
}

/// The sample backup shaped by `spec` (`crate::archive_testing::ArchiveSpec`): more messages,
/// another version in the header, or a frame that does not decode in the middle.
#[cfg(feature = "testing")]
pub(crate) fn write_shaped(
    path: &Path,
    ephemeral_key: &[u8; 32],
    own_aci: Aci,
    spec: &crate::archive_testing::ArchiveSpec,
) -> io::Result<()> {
    use backup::chat_item::{DirectionalDetails, IncomingMessageDetails, Item};

    let key = ArchiveKey::for_transfer(ephemeral_key, own_aci);
    let mut info = sample_info();
    info.version = spec.version;
    let mut writer = ArchiveWriter::new(&info);
    let mut frames = sample_frames();
    // A text that does not compress (a different run of hex digits in every message), so an
    // archive of thousands of messages is megabytes, not kilobytes. Synthetic.
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    for i in 0..spec.extra_messages {
        let mut body = String::with_capacity(768);
        for _ in 0..48 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            body.push_str(&format!("{seed:016x}"));
        }
        let sent = 4000 + u64::from(i);
        frames.push(Frame::ChatItem(Box::new(backup::ChatItem {
            chat_id: 1,
            author_id: 2,
            date_sent: sent,
            directional_details: Some(DirectionalDetails::Incoming(IncomingMessageDetails {
                date_received: sent + 1,
                read: true,
                sealed_sender: true,
                ..Default::default()
            })),
            item: Some(Item::StandardMessage(backup::StandardMessage {
                text: Some(backup::Text {
                    body,
                    body_ranges: Vec::new(),
                }),
                ..Default::default()
            })),
            ..Default::default()
        })));
    }
    for (i, frame) in frames.iter().enumerate() {
        if spec.broken_after == Some(u32::try_from(i).unwrap_or(u32::MAX)) {
            // A length-delimited field that claims more bytes than the frame has.
            writer.raw_frame(&[0x0a, 0x7f, 0x01]);
        }
        writer.frame(frame);
    }
    writer.write(path, &key)
}

/// A history to write as a transfer archive: plain data, so a test of another crate can
/// say what the phone had (`crate::archive_testing`). Synthetic; every name and text is the
/// test's own.
///
/// The frames, in order: the account, our own recipient (id 1), one recipient per contact
/// (ids from 2, in the order of [`contacts`](Self::contacts)), the generated contacts, the
/// release notes' recipient when asked for, one recipient per group (in the order of
/// [`groups`](Self::groups)), one chat per entry of [`chats`](Self::chats) (ids from 1), the
/// release notes' chat, then the items: chat after chat, or interleaved item by item, then
/// the orphans and the generated messages.
#[derive(Clone, Debug)]
pub struct HistorySpec {
    /// The people the phone knows. One that no chat names is a recipient without a chat.
    pub contacts: Vec<ContactSpec>,
    /// The chats. One without a text is a chat without an item.
    pub chats: Vec<ChatSpec>,
    /// The groups the phone knows. One that no chat names is a recipient without a chat.
    pub groups: Vec<GroupSpec>,
    /// Our ACI, as group snapshots and group changes name us ([`Party::Own`]);
    /// `write_history` fills it from the account it writes for when it is `None`.
    pub own_aci: Option<uuid::Uuid>,
    /// Our PNI ([`Party::OwnPni`]).
    pub own_pni: Option<uuid::Uuid>,
    /// Whether the items of the chats are written interleaved (the first of every chat, the
    /// second of every chat, …), as a phone writes them by received time over all chats,
    /// instead of chat after chat.
    pub interleaved: bool,
    /// Adds the release notes' recipient with a chat and this many items in it.
    pub release_notes_items: Option<u32>,
    /// Adds this many items that name a chat no frame describes.
    pub orphan_items: u32,
    /// Adds generated messages over generated chats.
    pub generated: Option<GeneratedSpec>,
    /// The format version the header states (the reader knows only 1).
    pub version: u64,
    /// After this many frames, one that is not a protobuf message, then the rest: the
    /// archive is authentic and fails half-way through its read.
    pub broken_after: Option<u32>,
    /// The phone's export time in ms (`BackupInfo.backupTimeMs`), the reference of the age
    /// rule for attachments. `None`: the writer's fixed time in 2023, far before any import,
    /// which a reader bounds to a day before its own time.
    pub export_time_ms: Option<u64>,
}

impl Default for HistorySpec {
    fn default() -> Self {
        Self {
            contacts: Vec::new(),
            chats: Vec::new(),
            groups: Vec::new(),
            own_aci: None,
            own_pni: None,
            interleaved: false,
            release_notes_items: None,
            orphan_items: 0,
            generated: None,
            version: SUPPORTED_VERSION,
            broken_after: None,
            export_time_ms: None,
        }
    }
}

/// A person of a [`HistorySpec`]. At least one of the three ids should be set; with none
/// the frame names nobody.
#[derive(Clone, Debug, Default)]
pub struct ContactSpec {
    /// The ACI.
    pub aci: Option<uuid::Uuid>,
    /// The PNI.
    pub pni: Option<uuid::Uuid>,
    /// The phone number as the archive holds it: a number without "+".
    pub e164: Option<u64>,
    /// The given name of the profile.
    pub profile_name: String,
    /// The given name in the phone's address book.
    pub system_name: String,
    /// The chat with the person is accepted on the phone.
    pub profile_sharing: bool,
    /// The person is blocked.
    pub blocked: bool,
    /// The profile key, when the archive holds one.
    pub profile_key: Option<[u8; 32]>,
}

/// A chat of a [`HistorySpec`], with the fields of its chat frame (all "nothing" by
/// default: `..ChatSpec::default()`).
#[derive(Clone, Debug)]
pub struct ChatSpec {
    /// Whom it is with.
    pub with: ChatWith,
    /// Its text messages, in the order the phone received them.
    pub texts: Vec<TextSpec>,
    /// `Chat.archived`.
    pub archived: bool,
    /// `Chat.markedUnread`.
    pub marked_unread: bool,
    /// `Chat.pinnedOrder` (ascending from 1 on Android; 0 is written as given).
    pub pinned_order: Option<u32>,
    /// `Chat.muteUntilMs` (`i64::MAX as u64`: for ever).
    pub mute_until_ms: Option<u64>,
    /// `Chat.expirationTimerMs`: the 1:1 timer in MILLISECONDS.
    pub expiration_timer_ms: Option<u64>,
    /// `Chat.expireTimerVersion` (Android writes it also without a timer).
    pub expire_timer_version: u32,
}

impl Default for ChatSpec {
    /// Note to Self without items, a chat frame that says nothing.
    fn default() -> Self {
        Self {
            with: ChatWith::NoteToSelf,
            texts: Vec::new(),
            archived: false,
            marked_unread: false,
            pinned_order: None,
            mute_until_ms: None,
            expiration_timer_ms: None,
            expire_timer_version: 0,
        }
    }
}

/// A group of a [`HistorySpec`].
#[derive(Clone, Debug, Default)]
pub struct GroupSpec {
    /// The master key (32 bytes; another length names no group).
    pub master_key: Vec<u8>,
    /// The group's message request was accepted on the phone.
    pub whitelisted: bool,
    /// The group is blocked.
    pub blocked: bool,
    /// The phone's snapshot; `None` writes the EMPTY snapshot of a phone without a state.
    pub snapshot: Option<SnapshotSpec>,
}

/// The phone's snapshot of a [`GroupSpec`].
#[derive(Clone, Debug, Default)]
pub struct SnapshotSpec {
    /// The title (a title blob is written also for `Some("")`; `None` writes none).
    pub title: Option<String>,
    /// The timer in SECONDS (`disappearingMessagesDuration`).
    pub timer_secs: Option<u32>,
    /// Full members, with whether each is an administrator.
    pub members: Vec<(Party, bool)>,
    /// Invited people (service id binaries: an ACI, or a PNI with its prefix).
    pub pending: Vec<Party>,
    /// People asking to join.
    pub requesting: Vec<Party>,
    /// The invite link's password (a secret of the group the import must never keep).
    pub invite_link_password: Vec<u8>,
}

/// A service id as snapshots and group changes hold it (raw bytes, not a recipient id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Party {
    /// Our ACI.
    Own,
    /// Our PNI (17 bytes).
    OwnPni,
    /// The ACI of the contact at this index of [`HistorySpec::contacts`].
    Contact(usize),
    /// This ACI, which may have no recipient frame.
    Aci(uuid::Uuid),
    /// This PNI (17 bytes).
    Pni(uuid::Uuid),
    /// The field is absent.
    Absent,
    /// The field is present and empty.
    Empty,
    /// Bytes that are no service id.
    Bad,
}

/// An entry of a group change ([`NoticeSpec::GroupChange`]).
#[derive(Clone, Debug)]
pub enum GroupChangeSpec {
    /// The group was created.
    Created {
        /// By whom.
        by: Party,
    },
    /// The name changed (`None`: removed).
    Name {
        /// By whom.
        by: Party,
        /// The new name.
        name: Option<String>,
    },
    /// The avatar changed.
    Avatar {
        /// By whom.
        by: Party,
    },
    /// The description changed.
    Description {
        /// By whom.
        by: Party,
    },
    /// The timer changed (ms).
    Timer {
        /// By whom.
        by: Party,
        /// The new timer in ms.
        expires_in_ms: u64,
    },
    /// A member left.
    MemberLeft {
        /// Who.
        who: Party,
    },
    /// A member was removed.
    MemberRemoved {
        /// By whom.
        by: Party,
        /// Who.
        removed: Party,
    },
    /// A member was added.
    MemberAdded {
        /// By whom.
        by: Party,
        /// Who.
        who: Party,
    },
    /// A member joined.
    MemberJoined {
        /// Who.
        who: Party,
    },
    /// A member joined by the link.
    JoinedByLink {
        /// Who.
        who: Party,
    },
    /// An invitation was accepted.
    InvitationAccepted {
        /// Who accepted.
        who: Party,
    },
    /// A join request was answered.
    JoinRequestApproval {
        /// The admin.
        by: Party,
        /// Who asked.
        requestor: Party,
        /// Approved (else rejected).
        approved: bool,
    },
    /// Nothing changed (a kind without a notice).
    Generic {
        /// By whom.
        by: Party,
    },
    /// An administrator was made (a kind without a notice).
    AdminStatus {
        /// By whom.
        by: Party,
        /// Who.
        member: Party,
    },
    /// We were invited (a kind without a notice).
    SelfInvited {
        /// By whom.
        inviter: Party,
    },
    /// An entry with nothing set.
    Unset,
}

/// Whom a chat of a [`HistorySpec`] is with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatWith {
    /// The contact at this index of [`HistorySpec::contacts`].
    Contact(usize),
    /// The group at this index of [`HistorySpec::groups`].
    Group(usize),
    /// Our own account: Note to Self.
    NoteToSelf,
    /// A recipient id no frame describes.
    Nobody,
}

/// A text message of a [`ChatSpec`].
#[derive(Clone, Debug, Default)]
pub struct TextSpec {
    /// Sent by us (else by the chat's contact).
    pub outgoing: bool,
    /// The text.
    pub body: String,
    /// The sender's time (ms).
    pub sent_at: u64,
    /// When the phone received it (ms; 0: unknown).
    pub received_at: u64,
    /// The server's time of an incoming message (ms).
    pub server_at: Option<u64>,
    /// An incoming message was read on the phone.
    pub read: bool,
    /// The send states of an outgoing message, one per entry, each for the chat's contact
    /// (or for ourselves in Note to Self).
    pub statuses: Vec<SendStatusSpec>,
    /// The length of its disappearing timer (ms).
    pub expires_in_ms: Option<u64>,
    /// When its timer started (ms).
    pub expire_start: Option<u64>,
    /// An incoming message names an author no frame describes.
    pub unknown_author: bool,
    /// What the item is: a text unless said otherwise. A notice or a call is written as a
    /// directionless update whose author is us when [`outgoing`](Self::outgoing) is set,
    /// else the chat's contact (or nobody, with [`unknown_author`](Self::unknown_author)).
    pub kind: ItemSpec,
    /// Formatting and mentions of the text, in UTF-16 units as the archive counts them.
    pub ranges: Vec<RangeSpec>,
    /// The message it quotes.
    pub quote: Option<QuoteSpec>,
    /// Its reactions, in the order they are written.
    pub reactions: Vec<ReactionSpec>,
    /// The item was edited: the sent times of its EARLIER versions, oldest first (the first
    /// is the original). [`sent_at`](Self::sent_at) is then the time of the newest edit, as
    /// the phone writes it.
    pub revisions: Vec<u64>,
    /// The author of an incoming item (or of a notice), in place of the chat's contact: in a
    /// group chat the member who wrote it.
    pub author: Option<Who>,
    /// The send states of an outgoing item by recipient (in a group one per member), in
    /// place of [`statuses`](Self::statuses).
    pub statuses_by: Vec<(Who, SendStatusSpec)>,
    /// The attachments of a text message (`StandardMessage.attachments`). With attachments
    /// an empty [`body`](Self::body) is written as no text (a message without a caption).
    pub attachments: Vec<AttachmentSpec>,
    /// The pointer to the rest of a long text (`StandardMessage.longText`), behind the body.
    pub long_text: Option<PointerSpec>,
}

/// One `MessageAttachment` of a [`TextSpec`].
#[derive(Clone, Debug, Default)]
pub struct AttachmentSpec {
    /// The file pointer (`None`: the attachment has none).
    pub pointer: Option<PointerSpec>,
    /// The archive's flag, an ENUM: 0 none, 1 voice message, 2 borderless, 3 GIF (any other
    /// number is written as it is).
    pub flag: i32,
    /// The client UUID.
    pub client_uuid: Option<[u8; 16]>,
}

/// A `FilePointer` as the archive holds it; `None` is an absent field.
#[derive(Clone, Debug, Default)]
pub struct PointerSpec {
    /// Content type.
    pub content_type: Option<String>,
    /// File name.
    pub file_name: Option<String>,
    /// Width.
    pub width: Option<u32>,
    /// Height.
    pub height: Option<u32>,
    /// Caption.
    pub caption: Option<String>,
    /// Blurhash.
    pub blur_hash: Option<String>,
    /// Waveform bars.
    pub audio_waveform: Option<Vec<u8>>,
    /// Duration in seconds.
    pub audio_duration_seconds: Option<f32>,
    /// `locatorInfo` (`None`: no locator).
    pub locator: Option<LocatorSpec>,
}

/// `FilePointer.LocatorInfo`, with raw bytes so that a wrong length can be written.
#[derive(Clone, Debug, Default)]
pub struct LocatorSpec {
    /// The 64-byte key (AES ‖ MAC).
    pub key: Vec<u8>,
    /// The integrity value.
    pub integrity: IntegritySpec,
    /// The plaintext size.
    pub size: u32,
    /// `transitCdnKey`.
    pub cdn_key: Option<String>,
    /// `transitCdnNumber`.
    pub cdn_number: Option<u32>,
    /// `transitTierUploadTimestamp` (ms).
    pub upload_timestamp: Option<u64>,
    /// `mediaTierCdnNumber`.
    pub media_tier_cdn_number: Option<u32>,
}

/// The integrity value of a [`LocatorSpec`].
#[derive(Clone, Debug, Default)]
pub enum IntegritySpec {
    /// None at all.
    #[default]
    Absent,
    /// `plaintextHash`.
    PlaintextHash(Vec<u8>),
    /// `encryptedDigest`.
    EncryptedDigest(Vec<u8>),
}

/// An attachment encrypted as a Signal client encrypts one ([`encrypt_attachment`]).
#[derive(Clone)]
pub struct EncryptedAttachment {
    /// The blob a CDN serves.
    pub blob: Vec<u8>,
    /// SHA-256 of the blob (a pointer's `encryptedDigest`).
    pub digest: [u8; 32],
    /// SHA-256 of the plaintext (a pointer's `plaintextHash`).
    pub plaintext_hash: [u8; 32],
    /// The plaintext size.
    pub size: u32,
}

impl std::fmt::Debug for EncryptedAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedAttachment")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// `plaintext` encrypted under `key` (AES ‖ MAC, 64 bytes) with `iv`, padded as Desktop pads.
pub fn encrypt_attachment(
    plaintext: &[u8],
    key: &[u8; 64],
    iv: [u8; 16],
) -> io::Result<EncryptedAttachment> {
    let other = |error: crate::attachment::AttachmentError| io::Error::other(error.to_string());
    let keys = AttachmentKeys::from_slice(&key[..]).map_err(other)?;
    let size = plaintext.len() as u64;
    let mut encryptor = Encryptor::new(&keys, iv, size, false).map_err(other)?;
    let mut blob = Vec::new();
    encryptor.update(plaintext, &mut blob).map_err(other)?;
    let done = encryptor.finish(&mut blob).map_err(other)?;
    Ok(EncryptedAttachment {
        blob,
        digest: done.digest,
        plaintext_hash: done.plaintext_hash,
        size: u32::try_from(size).map_err(io::Error::other)?,
    })
}

impl LocatorSpec {
    /// The locator of `encrypted` under `key` at `cdn_key` on CDN `cdn_number`, named by its
    /// plaintext hash (as the phone names a file it downloaded) or else by its digest.
    pub fn of(
        encrypted: &EncryptedAttachment,
        key: &[u8; 64],
        cdn_number: u32,
        cdn_key: &str,
        upload_timestamp: Option<u64>,
        by_plaintext_hash: bool,
    ) -> Self {
        Self {
            key: key.to_vec(),
            integrity: if by_plaintext_hash {
                IntegritySpec::PlaintextHash(encrypted.plaintext_hash.to_vec())
            } else {
                IntegritySpec::EncryptedDigest(encrypted.digest.to_vec())
            },
            size: encrypted.size,
            cdn_key: Some(cdn_key.to_owned()),
            cdn_number: Some(cdn_number),
            upload_timestamp,
            media_tier_cdn_number: None,
        }
    }
}

impl PointerSpec {
    pub(crate) fn to_proto(&self) -> backup::FilePointer {
        use backup::file_pointer::locator_info::IntegrityCheck;
        backup::FilePointer {
            content_type: self.content_type.clone(),
            file_name: self.file_name.clone(),
            width: self.width,
            height: self.height,
            caption: self.caption.clone(),
            blur_hash: self.blur_hash.clone(),
            audio_waveform: self.audio_waveform.clone(),
            audio_duration_seconds: self.audio_duration_seconds,
            locator_info: self
                .locator
                .as_ref()
                .map(|l| backup::file_pointer::LocatorInfo {
                    key: l.key.clone(),
                    integrity_check: match &l.integrity {
                        IntegritySpec::Absent => None,
                        IntegritySpec::PlaintextHash(h) => {
                            Some(IntegrityCheck::PlaintextHash(h.clone()))
                        }
                        IntegritySpec::EncryptedDigest(d) => {
                            Some(IntegrityCheck::EncryptedDigest(d.clone()))
                        }
                    },
                    size: l.size,
                    transit_cdn_key: l.cdn_key.clone(),
                    transit_cdn_number: l.cdn_number,
                    transit_tier_upload_timestamp: l.upload_timestamp,
                    media_tier_cdn_number: l.media_tier_cdn_number,
                    local_key: None,
                }),
            ..Default::default()
        }
    }
}

/// What an item of a [`ChatSpec`] is.
#[derive(Clone, Debug, Default)]
pub enum ItemSpec {
    /// A text message (the default).
    #[default]
    Text,
    /// A sticker; `data`: with the image's pointer (else it has none).
    Sticker {
        /// The sticker has its image's pointer.
        data: bool,
    },
    /// A sticker with this image pointer.
    StickerWith {
        /// The image's pointer.
        pointer: PointerSpec,
    },
    /// A sticker whose image pointer states this content type (`None`: it states none).
    StickerTyped {
        /// The content type of the pointer; a long or an empty one tests the bounds.
        content_type: Option<String>,
    },
    /// A view-once message whose media was not viewed yet (its pointer is in the archive).
    ViewOnce,
    /// A shared contact card with this name.
    ContactCard {
        /// The card's name (a long one makes a large item).
        name: String,
    },
    /// A payment notification.
    Payment,
    /// A gift badge.
    GiftBadge,
    /// A poll with this question.
    Poll {
        /// The question.
        question: String,
    },
    /// A reply to a story, with the item's text.
    StoryReply,
    /// A notice of the chat (a directionless update).
    Notice(NoticeSpec),
    /// A call (a directionless update).
    Call(CallSpec),
}

/// A notice of a [`ItemSpec::Notice`].
#[derive(Clone, Debug)]
pub enum NoticeSpec {
    /// The 1:1 disappearing timer was changed to this many ms (0: off).
    TimerChange {
        /// The new timer.
        expires_in_ms: u64,
    },
    /// A simple update.
    Simple(SimpleSpec),
    /// A poll was ended; it was the message sent at `target`.
    PollTerminate {
        /// The poll's sent time.
        target: u64,
    },
    /// A message was pinned: the one sent at `target`.
    PinMessage {
        /// The pinned message's sent time.
        target: u64,
    },
    /// A profile name changed.
    ProfileChange,
    /// Two chats of one person were merged.
    ThreadMerge,
    /// A chat moved from a number to an account.
    SessionSwitchover,
    /// A profile name was learned.
    LearnedProfile,
    /// An update with nothing set (a newer phone's kind).
    Unset,
    /// A group change with these entries (in a group chat).
    GroupChange(Vec<GroupChangeSpec>),
}

/// The kinds of `SimpleChatUpdate`, with a number of the proto for one this list lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimpleSpec {
    /// The contact joined Signal.
    JoinedSignal,
    /// The contact's safety number changed.
    IdentityUpdate,
    /// The contact was marked verified.
    IdentityVerified,
    /// The contact was marked unverified.
    IdentityDefault,
    /// The contact changed their number.
    ChangeNumber,
    /// A donation request of the release notes.
    ReleaseChannelDonationRequest,
    /// The session was reset.
    EndSession,
    /// The session was refreshed.
    ChatSessionRefresh,
    /// A message could not be decrypted.
    BadDecrypt,
    /// Payments were activated.
    PaymentsActivated,
    /// A request to activate payments.
    PaymentActivationRequest,
    /// A message of a newer protocol.
    UnsupportedProtocolMessage,
    /// The chat was reported as spam.
    ReportedSpam,
    /// The contact was blocked.
    Blocked,
    /// The contact was unblocked.
    Unblocked,
    /// The message request was accepted.
    MessageRequestAccepted,
    /// The proto's number for a kind.
    Number(i32),
}

/// A call of an [`ItemSpec::Call`].
#[derive(Clone, Copy, Debug)]
pub enum CallSpec {
    /// A 1:1 call.
    Individual {
        /// The call's id, when the archive has one.
        call_id: Option<u64>,
        /// A video call (else voice).
        video: bool,
        /// We called (else the contact did).
        outgoing: bool,
        /// What became of it.
        state: IndividualCallSpec,
        /// The phone's user saw it.
        read: bool,
        /// When it started (ms; 0: not stated).
        started_at: u64,
    },
    /// A group call.
    Group {
        /// The call's id, when the archive has one.
        call_id: Option<u64>,
        /// What became of it.
        state: GroupCallSpec,
        /// Who rang.
        ringer: Option<Who>,
        /// Who started it.
        started_by: Option<Who>,
        /// The phone's user saw it.
        read: bool,
        /// When it started (ms; 0: not stated).
        started_at: u64,
    },
}

/// `IndividualCall.State`, with a number of the proto for one this list lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndividualCallSpec {
    /// The state 0 (read as accepted).
    Unknown,
    /// Accepted.
    Accepted,
    /// Not accepted.
    NotAccepted,
    /// Missed.
    Missed,
    /// Declined by a notification profile.
    MissedNotificationProfile,
    /// The proto's number for a state.
    Number(i32),
}

/// `GroupCall.State`, with a number of the proto for one this list lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupCallSpec {
    /// The state 0 (read as generic).
    Unknown,
    /// Started without ringing.
    Generic,
    /// We joined it.
    Joined,
    /// It rings.
    Ringing,
    /// We accepted the ring.
    Accepted,
    /// We declined the ring.
    Declined,
    /// We missed the ring.
    Missed,
    /// Declined by a notification profile.
    MissedNotificationProfile,
    /// We rang.
    OutgoingRing,
    /// The proto's number for a state.
    Number(i32),
}

/// Who a quote, a reaction or a call names, in the chat of the item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// Our own account.
    Own,
    /// The chat's contact.
    Peer,
    /// A recipient id no frame describes.
    Unknown,
    /// The contact at this index of [`HistorySpec::contacts`] (in a 1:1 chat: somebody else).
    Contact(usize),
}

/// A formatting or mention range of a text, in UTF-16 units.
#[derive(Clone, Copy, Debug)]
pub struct RangeSpec {
    /// Where it starts.
    pub start: u32,
    /// How long it is.
    pub length: u32,
    /// What it is.
    pub kind: RangeKindSpec,
}

/// What a [`RangeSpec`] is.
#[derive(Clone, Copy, Debug)]
pub enum RangeKindSpec {
    /// Bold.
    Bold,
    /// Italic.
    Italic,
    /// Spoiler.
    Spoiler,
    /// Strikethrough.
    Strikethrough,
    /// Monospace.
    Monospace,
    /// The style 0, which importers ignore.
    NoStyle,
    /// A mention of this ACI.
    Mention(uuid::Uuid),
    /// A mention whose ACI is not 16 bytes.
    BadMention,
}

/// A quote of a [`TextSpec`].
#[derive(Clone, Debug, Default)]
pub struct QuoteSpec {
    /// The quoted message's sent time; `None`: the phone did not find it.
    pub target: Option<u64>,
    /// Who wrote the quoted message.
    pub author: Option<Who>,
    /// The quoted text.
    pub text: String,
    /// Its ranges.
    pub ranges: Vec<RangeSpec>,
    /// The first quoted attachment: its type and file name.
    pub attachment: Option<(String, String)>,
}

/// A reaction of a [`TextSpec`].
#[derive(Clone, Debug)]
pub struct ReactionSpec {
    /// Who reacted.
    pub by: Who,
    /// The emoji (or whatever the archive holds).
    pub emoji: String,
    /// When (ms).
    pub sent_at: u64,
    /// Its place among the reactions (higher is more recent).
    pub sort_order: u64,
}

/// One recipient's state of an outgoing message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendStatusSpec {
    /// Not sent yet.
    Pending,
    /// Sent.
    Sent,
    /// Delivered.
    Delivered,
    /// Read.
    Read,
    /// Viewed.
    Viewed,
    /// Not sent to this recipient on purpose.
    Skipped,
    /// Failed, the network's fault.
    FailedNetwork,
    /// Failed, the recipient's identity key changed.
    FailedIdentity,
}

/// Generated content of a [`HistorySpec`]: `chats` more contacts with a chat each, and
/// `messages` texts dealt out over those chats one by one (so their items interleave),
/// every third one outgoing and delivered, the others incoming and read. Their sent times
/// rise by one second from [`GeneratedSpec::first_sent_at`].
#[derive(Clone, Debug)]
pub struct GeneratedSpec {
    /// How many chats.
    pub chats: u32,
    /// How many messages in all.
    pub messages: u32,
    /// What every text starts with (its number follows).
    pub text: String,
    /// The sent time of the first message (ms).
    pub first_sent_at: u64,
}

/// The ACI of the `i`-th generated contact of a [`GeneratedSpec`].
pub fn generated_aci(i: u32) -> uuid::Uuid {
    let mut bytes = [0x9e; 16];
    bytes[..4].copy_from_slice(&i.to_be_bytes());
    // A version-4 UUID, as an ACI is.
    bytes[6] = 0x40 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    uuid::Uuid::from_bytes(bytes)
}

/// The frames of `spec` after its `BackupInfo`.
pub(crate) fn history_frames(spec: &HistorySpec) -> Vec<Frame> {
    use backup::chat_item::{DirectionalDetails, IncomingMessageDetails, OutgoingMessageDetails};
    use backup::recipient::Destination;
    use backup::send_status::DeliveryStatus;

    const SELF_ID: u64 = 1;
    const NOBODY: u64 = 4_000_000_000;
    let recipient = |id, destination| {
        Frame::Recipient(backup::Recipient {
            id,
            destination: Some(destination),
        })
    };
    let contact = |c: &ContactSpec| {
        Destination::Contact(backup::Contact {
            aci: c.aci.map(|aci| aci.as_bytes().to_vec()),
            pni: c.pni.map(|pni| pni.as_bytes().to_vec()),
            e164: c.e164,
            blocked: c.blocked,
            profile_sharing: c.profile_sharing,
            profile_key: c.profile_key.map(|key| key.to_vec()),
            profile_given_name: (!c.profile_name.is_empty()).then(|| c.profile_name.clone()),
            system_given_name: c.system_name.clone(),
            registration: Some(backup::contact::Registration::Registered(
                backup::contact::Registered {},
            )),
            ..Default::default()
        })
    };
    let status = |recipient_id, s: SendStatusSpec| backup::SendStatus {
        recipient_id,
        timestamp: 0,
        delivery_status: Some(match s {
            SendStatusSpec::Pending => DeliveryStatus::Pending(Default::default()),
            SendStatusSpec::Sent => DeliveryStatus::Sent(Default::default()),
            SendStatusSpec::Delivered => DeliveryStatus::Delivered(Default::default()),
            SendStatusSpec::Read => DeliveryStatus::Read(Default::default()),
            SendStatusSpec::Viewed => DeliveryStatus::Viewed(Default::default()),
            SendStatusSpec::Skipped => DeliveryStatus::Skipped(Default::default()),
            SendStatusSpec::FailedNetwork => DeliveryStatus::Failed(backup::send_status::Failed {
                reason: backup::send_status::failed::FailureReason::Network as i32,
            }),
            SendStatusSpec::FailedIdentity => DeliveryStatus::Failed(backup::send_status::Failed {
                reason: backup::send_status::failed::FailureReason::IdentityKeyMismatch as i32,
            }),
        }),
    };
    let contact_id = |index: usize| 2 + index as u64;
    let own_aci = spec.own_aci.unwrap_or_default();
    let own_pni = spec.own_pni.unwrap_or_default();
    let party = |p: Party| -> Option<Vec<u8>> {
        let pni = |uuid: uuid::Uuid| {
            let mut bytes = vec![0x01];
            bytes.extend_from_slice(uuid.as_bytes());
            bytes
        };
        match p {
            Party::Own => Some(own_aci.as_bytes().to_vec()),
            Party::OwnPni => Some(pni(own_pni)),
            Party::Contact(i) => Some(spec.contacts[i].aci.unwrap_or_default().as_bytes().to_vec()),
            Party::Aci(uuid) => Some(uuid.as_bytes().to_vec()),
            Party::Pni(uuid) => Some(pni(uuid)),
            Party::Absent => None,
            Party::Empty => Some(Vec::new()),
            Party::Bad => Some(vec![7; 5]),
        }
    };
    // `peer` is the recipient id of the chat's other side (ourselves in Note to Self).
    let text = |chat_id: u64, peer: u64, t: &TextSpec| {
        let directionless = matches!(t.kind, ItemSpec::Notice(_) | ItemSpec::Call(_));
        let details = if directionless {
            DirectionalDetails::Directionless(Default::default())
        } else if t.outgoing {
            DirectionalDetails::Outgoing(OutgoingMessageDetails {
                send_status: t.statuses.iter().map(|s| status(peer, *s)).collect(),
                date_received: t.received_at,
            })
        } else {
            DirectionalDetails::Incoming(IncomingMessageDetails {
                date_received: t.received_at,
                date_server_sent: t.server_at,
                read: t.read,
                sealed_sender: true,
            })
        };
        let who = |who: Who| match who {
            Who::Own => SELF_ID,
            Who::Peer => peer,
            Who::Unknown => NOBODY,
            Who::Contact(index) => contact_id(index),
        };
        let details = match (&details, t.statuses_by.is_empty()) {
            (DirectionalDetails::Outgoing(out), false) => {
                DirectionalDetails::Outgoing(OutgoingMessageDetails {
                    send_status: t
                        .statuses_by
                        .iter()
                        .map(|(w, s)| status(who(*w), *s))
                        .collect(),
                    date_received: out.date_received,
                })
            }
            _ => details,
        };
        let mut item = backup::ChatItem {
            chat_id,
            author_id: match (t.outgoing, t.unknown_author, t.author) {
                (true, _, _) => SELF_ID,
                (false, true, _) => NOBODY,
                (false, false, Some(author)) => who(author),
                (false, false, None) => peer,
            },
            date_sent: t.sent_at,
            expire_start_date: t.expire_start,
            expires_in_ms: t.expires_in_ms,
            directional_details: Some(details),
            item: Some(item_content(t, &who, &party)),
            ..Default::default()
        };
        // The earlier versions of an edited item: the same item at each earlier time.
        item.revisions = t
            .revisions
            .iter()
            .map(|at| backup::ChatItem {
                date_sent: *at,
                revisions: Vec::new(),
                ..item.clone()
            })
            .collect();
        Frame::ChatItem(Box::new(item))
    };

    let mut frames = vec![
        Frame::Account(Box::new(backup::AccountData {
            profile_key: vec![1; 32],
            given_name: "Test".to_owned(),
            ..Default::default()
        })),
        recipient(SELF_ID, Destination::Self_(backup::Self_::default())),
    ];
    for (index, c) in spec.contacts.iter().enumerate() {
        frames.push(recipient(contact_id(index), contact(c)));
    }
    let mut next_recipient = contact_id(spec.contacts.len());
    let generated_first = next_recipient;
    if let Some(generated) = &spec.generated {
        for i in 0..generated.chats {
            frames.push(recipient(
                next_recipient,
                contact(&ContactSpec {
                    aci: Some(generated_aci(i)),
                    profile_name: format!("Generated {i}"),
                    profile_sharing: true,
                    ..ContactSpec::default()
                }),
            ));
            next_recipient += 1;
        }
    }
    let release_notes = next_recipient;
    if spec.release_notes_items.is_some() {
        frames.push(recipient(
            release_notes,
            Destination::ReleaseNotes(backup::ReleaseNotes {}),
        ));
    }
    let group_id = |index: usize| release_notes + 1 + index as u64;
    for (index, g) in spec.groups.iter().enumerate() {
        use backup::group::group_attribute_blob::Content;
        use backup::group::{
            GroupAttributeBlob, GroupSnapshot, Member, MemberPendingAdminApproval,
            MemberPendingProfileKey,
        };
        let snapshot = g
            .snapshot
            .as_ref()
            .map_or_else(GroupSnapshot::default, |snap| GroupSnapshot {
                title: snap.title.clone().map(|title| GroupAttributeBlob {
                    content: Some(Content::Title(title)),
                }),
                disappearing_messages_timer: snap.timer_secs.map(|secs| GroupAttributeBlob {
                    content: Some(Content::DisappearingMessagesDuration(secs)),
                }),
                version: 3,
                members: snap
                    .members
                    .iter()
                    .map(|(p, admin)| Member {
                        user_id: party(*p).unwrap_or_default(),
                        role: if *admin {
                            backup::group::member::Role::Administrator
                        } else {
                            backup::group::member::Role::Default
                        } as i32,
                        ..Default::default()
                    })
                    .collect(),
                members_pending_profile_key: snap
                    .pending
                    .iter()
                    .map(|p| MemberPendingProfileKey {
                        member: Some(Member {
                            user_id: party(*p).unwrap_or_default(),
                            ..Default::default()
                        }),
                        added_by_user_id: own_aci.as_bytes().to_vec(),
                        timestamp: 1,
                    })
                    .collect(),
                members_pending_admin_approval: snap
                    .requesting
                    .iter()
                    .map(|p| MemberPendingAdminApproval {
                        user_id: party(*p).unwrap_or_default(),
                        timestamp: 1,
                    })
                    .collect(),
                invite_link_password: snap.invite_link_password.clone(),
                ..Default::default()
            });
        frames.push(recipient(
            group_id(index),
            Destination::Group(backup::Group {
                master_key: g.master_key.clone(),
                whitelisted: g.whitelisted,
                blocked: g.blocked,
                snapshot: Some(snapshot),
                ..Default::default()
            }),
        ));
    }

    let chat = |id, recipient_id| {
        Frame::Chat(backup::Chat {
            id,
            recipient_id,
            ..Default::default()
        })
    };
    let peer_of = |with: ChatWith| match with {
        ChatWith::Contact(index) => contact_id(index),
        ChatWith::Group(index) => group_id(index),
        ChatWith::NoteToSelf => SELF_ID,
        ChatWith::Nobody => NOBODY,
    };
    let chat_id = |index: usize| 1 + index as u64;
    for (index, c) in spec.chats.iter().enumerate() {
        frames.push(Frame::Chat(backup::Chat {
            id: chat_id(index),
            recipient_id: peer_of(c.with),
            archived: c.archived,
            marked_unread: c.marked_unread,
            pinned_order: c.pinned_order,
            mute_until_ms: c.mute_until_ms,
            expiration_timer_ms: c.expiration_timer_ms,
            expire_timer_version: c.expire_timer_version,
            ..Default::default()
        }));
    }
    let mut next_chat = chat_id(spec.chats.len());
    let generated_first_chat = next_chat;
    if let Some(generated) = &spec.generated {
        for i in 0..u64::from(generated.chats) {
            frames.push(chat(next_chat, generated_first + i));
            next_chat += 1;
        }
    }
    let release_chat = next_chat;
    if spec.release_notes_items.is_some() {
        frames.push(chat(release_chat, release_notes));
    }

    // The items.
    if spec.interleaved {
        let longest = spec.chats.iter().map(|c| c.texts.len()).max().unwrap_or(0);
        for position in 0..longest {
            for (index, c) in spec.chats.iter().enumerate() {
                if let Some(t) = c.texts.get(position) {
                    frames.push(text(chat_id(index), peer_of(c.with), t));
                }
            }
        }
    } else {
        for (index, c) in spec.chats.iter().enumerate() {
            for t in &c.texts {
                frames.push(text(chat_id(index), peer_of(c.with), t));
            }
        }
    }
    for i in 0..spec.release_notes_items.unwrap_or(0) {
        frames.push(text(
            release_chat,
            release_notes,
            &TextSpec {
                body: "release notes".to_owned(),
                sent_at: 1_000 + u64::from(i),
                received_at: 1_001 + u64::from(i),
                read: true,
                ..TextSpec::default()
            },
        ));
    }
    for i in 0..spec.orphan_items {
        frames.push(text(
            NOBODY + 1 + u64::from(i),
            SELF_ID,
            &TextSpec {
                outgoing: true,
                body: "orphan".to_owned(),
                sent_at: 2_000 + u64::from(i),
                ..TextSpec::default()
            },
        ));
    }
    if let Some(generated) = &spec.generated {
        let chats = u64::from(generated.chats.max(1));
        for i in 0..u64::from(generated.messages) {
            let slot = i % chats;
            let sent_at = generated.first_sent_at + i * 1_000;
            let outgoing = i % 3 == 0;
            frames.push(text(
                generated_first_chat + slot,
                generated_first + slot,
                &TextSpec {
                    outgoing,
                    body: format!("{} {i}", generated.text),
                    sent_at,
                    received_at: sent_at + 1,
                    read: true,
                    statuses: if outgoing {
                        vec![SendStatusSpec::Delivered]
                    } else {
                        Vec::new()
                    },
                    ..TextSpec::default()
                },
            ));
        }
    }
    frames
}

/// A sticker message with the image's pointer (`None`: it has none).
fn sticker_item(
    data: Option<backup::FilePointer>,
    reactions: Vec<backup::Reaction>,
) -> backup::chat_item::Item {
    backup::chat_item::Item::StickerMessage(backup::StickerMessage {
        sticker: Some(backup::Sticker {
            pack_id: vec![5; 16],
            pack_key: vec![6; 32],
            sticker_id: 7,
            emoji: Some("\u{1f600}".to_owned()),
            data,
        }),
        reactions,
    })
}

/// The content of the item `t`; `who` gives the recipient id a [`Who`] stands for.
fn item_content(
    t: &TextSpec,
    who: &dyn Fn(Who) -> u64,
    party: &dyn Fn(Party) -> Option<Vec<u8>>,
) -> backup::chat_item::Item {
    use backup::chat_item::Item;
    use backup::chat_update_message::Update;
    let ranges = |ranges: &[RangeSpec]| -> Vec<backup::BodyRange> {
        use backup::body_range::{AssociatedValue, Style};
        ranges
            .iter()
            .map(|r| backup::BodyRange {
                start: r.start,
                length: r.length,
                associated_value: Some(match r.kind {
                    RangeKindSpec::Bold => AssociatedValue::Style(Style::Bold as i32),
                    RangeKindSpec::Italic => AssociatedValue::Style(Style::Italic as i32),
                    RangeKindSpec::Spoiler => AssociatedValue::Style(Style::Spoiler as i32),
                    RangeKindSpec::Strikethrough => {
                        AssociatedValue::Style(Style::Strikethrough as i32)
                    }
                    RangeKindSpec::Monospace => AssociatedValue::Style(Style::Monospace as i32),
                    RangeKindSpec::NoStyle => AssociatedValue::Style(Style::None as i32),
                    RangeKindSpec::Mention(aci) => {
                        AssociatedValue::MentionAci(aci.as_bytes().to_vec())
                    }
                    RangeKindSpec::BadMention => AssociatedValue::MentionAci(vec![1, 2, 3]),
                }),
            })
            .collect()
    };
    let text = || backup::Text {
        body: t.body.clone(),
        body_ranges: ranges(&t.ranges),
    };
    let reactions: Vec<backup::Reaction> = t
        .reactions
        .iter()
        .map(|r| backup::Reaction {
            emoji: r.emoji.clone(),
            author_id: who(r.by),
            sent_timestamp: r.sent_at,
            sort_order: r.sort_order,
        })
        .collect();
    // A pointer of the transit tier, as the phone writes one it has not uploaded again.
    let pointer = || backup::FilePointer {
        content_type: Some("image/webp".to_owned()),
        width: Some(512),
        height: Some(512),
        locator_info: Some(backup::file_pointer::LocatorInfo {
            key: vec![3; 64],
            integrity_check: Some(
                backup::file_pointer::locator_info::IntegrityCheck::PlaintextHash(vec![4; 32]),
            ),
            size: 2_048,
            transit_cdn_key: Some("sticker-cdn-key".to_owned()),
            transit_cdn_number: Some(3),
            ..Default::default()
        }),
        ..Default::default()
    };
    let update = |update: Update| {
        Item::UpdateMessage(backup::ChatUpdateMessage {
            update: Some(update),
        })
    };
    match &t.kind {
        ItemSpec::Text => Item::StandardMessage(backup::StandardMessage {
            text: (!t.body.is_empty() || t.attachments.is_empty()).then(text),
            attachments: t
                .attachments
                .iter()
                .map(|a| backup::MessageAttachment {
                    pointer: a.pointer.as_ref().map(PointerSpec::to_proto),
                    flag: a.flag,
                    was_downloaded: false,
                    client_uuid: a.client_uuid.map(|u| u.to_vec()),
                })
                .collect(),
            long_text: t.long_text.as_ref().map(PointerSpec::to_proto),
            quote: t.quote.as_ref().map(|q| backup::Quote {
                target_sent_timestamp: q.target,
                author_id: q.author.map_or(0, who),
                text: Some(backup::Text {
                    body: q.text.clone(),
                    body_ranges: ranges(&q.ranges),
                }),
                attachments: q
                    .attachment
                    .iter()
                    .map(|(mime, name)| backup::quote::QuotedAttachment {
                        content_type: Some(mime.clone()),
                        file_name: Some(name.clone()),
                        thumbnail: None,
                    })
                    .collect(),
                r#type: backup::quote::Type::Normal as i32,
            }),
            reactions,
            ..Default::default()
        }),
        ItemSpec::Sticker { data } => sticker_item(data.then(pointer), reactions),
        ItemSpec::StickerWith { pointer } => sticker_item(Some(pointer.to_proto()), reactions),
        ItemSpec::StickerTyped { content_type } => sticker_item(
            Some(backup::FilePointer {
                content_type: content_type.clone(),
                ..pointer()
            }),
            reactions,
        ),
        ItemSpec::ViewOnce => Item::ViewOnceMessage(backup::ViewOnceMessage {
            attachment: Some(backup::MessageAttachment {
                pointer: Some(backup::FilePointer {
                    content_type: Some("image/jpeg".to_owned()),
                    ..pointer()
                }),
                ..Default::default()
            }),
            reactions,
        }),
        ItemSpec::ContactCard { name } => Item::ContactMessage(backup::ContactMessage {
            contact: Some(backup::ContactAttachment {
                name: Some(backup::contact_attachment::Name {
                    given_name: name.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            reactions,
        }),
        ItemSpec::Payment => Item::PaymentNotification(backup::PaymentNotification {
            amount_mob: Some("1.5".to_owned()),
            note: Some(t.body.clone()),
            ..Default::default()
        }),
        ItemSpec::GiftBadge => Item::GiftBadge(backup::GiftBadge {
            receipt_credential_presentation: vec![8; 32],
            state: backup::gift_badge::State::Opened as i32,
        }),
        ItemSpec::Poll { question } => Item::Poll(backup::Poll {
            question: question.clone(),
            options: ["yes", "no"]
                .into_iter()
                .map(|option| backup::poll::PollOption {
                    option: option.to_owned(),
                    votes: Vec::new(),
                })
                .collect(),
            reactions,
            ..Default::default()
        }),
        ItemSpec::StoryReply => Item::DirectStoryReplyMessage(backup::DirectStoryReplyMessage {
            reply: Some(backup::direct_story_reply_message::Reply::TextReply(
                backup::direct_story_reply_message::TextReply {
                    text: Some(text()),
                    long_text: None,
                },
            )),
            reactions,
        }),
        ItemSpec::Notice(notice) => match notice {
            NoticeSpec::TimerChange { expires_in_ms } => update(Update::ExpirationTimerChange(
                backup::ExpirationTimerChatUpdate {
                    expires_in_ms: *expires_in_ms,
                },
            )),
            NoticeSpec::Simple(kind) => update(Update::SimpleUpdate(backup::SimpleChatUpdate {
                r#type: simple_number(*kind),
            })),
            NoticeSpec::PollTerminate { target } => {
                update(Update::PollTerminate(backup::PollTerminateUpdate {
                    target_sent_timestamp: *target,
                    question: "a question".to_owned(),
                }))
            }
            NoticeSpec::PinMessage { target } => {
                update(Update::PinMessage(backup::PinMessageUpdate {
                    target_sent_timestamp: *target,
                    author_id: who(Who::Peer),
                }))
            }
            NoticeSpec::ProfileChange => update(Update::ProfileChange(Default::default())),
            NoticeSpec::ThreadMerge => update(Update::ThreadMerge(Default::default())),
            NoticeSpec::SessionSwitchover => update(Update::SessionSwitchover(Default::default())),
            NoticeSpec::LearnedProfile => update(Update::LearnedProfileChange(Default::default())),
            NoticeSpec::Unset => Item::UpdateMessage(backup::ChatUpdateMessage { update: None }),
            NoticeSpec::GroupChange(entries) => {
                use backup::group_change_chat_update::Update as Entry;
                use backup::group_change_chat_update::update::Update as E;
                let req = |p: Party| party(p).unwrap_or_default();
                update(Update::GroupChange(backup::GroupChangeChatUpdate {
                    updates: entries
                        .iter()
                        .map(|entry| Entry {
                            update: match entry {
                                GroupChangeSpec::Created { by } => {
                                    Some(E::GroupCreationUpdate(backup::GroupCreationUpdate {
                                        updater_aci: party(*by),
                                    }))
                                }
                                GroupChangeSpec::Name { by, name } => {
                                    Some(E::GroupNameUpdate(backup::GroupNameUpdate {
                                        updater_aci: party(*by),
                                        new_group_name: name.clone(),
                                    }))
                                }
                                GroupChangeSpec::Avatar { by } => {
                                    Some(E::GroupAvatarUpdate(backup::GroupAvatarUpdate {
                                        updater_aci: party(*by),
                                        was_removed: false,
                                    }))
                                }
                                GroupChangeSpec::Description { by } => Some(
                                    E::GroupDescriptionUpdate(backup::GroupDescriptionUpdate {
                                        updater_aci: party(*by),
                                        new_description: Some("a description".to_owned()),
                                    }),
                                ),
                                GroupChangeSpec::Timer { by, expires_in_ms } => {
                                    Some(E::GroupExpirationTimerUpdate(
                                        backup::GroupExpirationTimerUpdate {
                                            expires_in_ms: *expires_in_ms,
                                            updater_aci: party(*by),
                                        },
                                    ))
                                }
                                GroupChangeSpec::MemberLeft { who } => {
                                    Some(E::GroupMemberLeftUpdate(backup::GroupMemberLeftUpdate {
                                        aci: req(*who),
                                    }))
                                }
                                GroupChangeSpec::MemberRemoved { by, removed } => Some(
                                    E::GroupMemberRemovedUpdate(backup::GroupMemberRemovedUpdate {
                                        remover_aci: party(*by),
                                        removed_aci: req(*removed),
                                    }),
                                ),
                                GroupChangeSpec::MemberAdded { by, who } => Some(
                                    E::GroupMemberAddedUpdate(backup::GroupMemberAddedUpdate {
                                        updater_aci: party(*by),
                                        new_member_aci: req(*who),
                                        had_open_invitation: false,
                                        inviter_aci: None,
                                    }),
                                ),
                                GroupChangeSpec::MemberJoined { who } => Some(
                                    E::GroupMemberJoinedUpdate(backup::GroupMemberJoinedUpdate {
                                        new_member_aci: req(*who),
                                    }),
                                ),
                                GroupChangeSpec::JoinedByLink { who } => {
                                    Some(E::GroupMemberJoinedByLinkUpdate(
                                        backup::GroupMemberJoinedByLinkUpdate {
                                            new_member_aci: req(*who),
                                        },
                                    ))
                                }
                                GroupChangeSpec::InvitationAccepted { who } => {
                                    Some(E::GroupInvitationAcceptedUpdate(
                                        backup::GroupInvitationAcceptedUpdate {
                                            inviter_aci: None,
                                            new_member_aci: req(*who),
                                        },
                                    ))
                                }
                                GroupChangeSpec::JoinRequestApproval {
                                    by,
                                    requestor,
                                    approved,
                                } => Some(E::GroupJoinRequestApprovalUpdate(
                                    backup::GroupJoinRequestApprovalUpdate {
                                        requestor_aci: req(*requestor),
                                        updater_aci: party(*by),
                                        was_approved: *approved,
                                    },
                                )),
                                GroupChangeSpec::Generic { by } => {
                                    Some(E::GenericGroupUpdate(backup::GenericGroupUpdate {
                                        updater_aci: party(*by),
                                    }))
                                }
                                GroupChangeSpec::AdminStatus { by, member } => Some(
                                    E::GroupAdminStatusUpdate(backup::GroupAdminStatusUpdate {
                                        updater_aci: party(*by),
                                        member_aci: req(*member),
                                        was_admin_status_granted: true,
                                    }),
                                ),
                                GroupChangeSpec::SelfInvited { inviter } => Some(
                                    E::SelfInvitedToGroupUpdate(backup::SelfInvitedToGroupUpdate {
                                        inviter_aci: party(*inviter),
                                    }),
                                ),
                                GroupChangeSpec::Unset => None,
                            },
                        })
                        .collect(),
                }))
            }
        },
        ItemSpec::Call(CallSpec::Individual {
            call_id,
            video,
            outgoing,
            state,
            read,
            started_at,
        }) => {
            use backup::individual_call::{Direction, State, Type};
            update(Update::IndividualCall(backup::IndividualCall {
                call_id: *call_id,
                r#type: if *video {
                    Type::VideoCall
                } else {
                    Type::AudioCall
                } as i32,
                direction: if *outgoing {
                    Direction::Outgoing
                } else {
                    Direction::Incoming
                } as i32,
                state: match state {
                    IndividualCallSpec::Unknown => State::UnknownState as i32,
                    IndividualCallSpec::Accepted => State::Accepted as i32,
                    IndividualCallSpec::NotAccepted => State::NotAccepted as i32,
                    IndividualCallSpec::Missed => State::Missed as i32,
                    IndividualCallSpec::MissedNotificationProfile => {
                        State::MissedNotificationProfile as i32
                    }
                    IndividualCallSpec::Number(n) => *n,
                },
                started_call_timestamp: *started_at,
                read: *read,
            }))
        }
        ItemSpec::Call(CallSpec::Group {
            call_id,
            state,
            ringer,
            started_by,
            read,
            started_at,
        }) => {
            use backup::group_call::State;
            update(Update::GroupCall(backup::GroupCall {
                call_id: *call_id,
                state: match state {
                    GroupCallSpec::Unknown => State::UnknownState as i32,
                    GroupCallSpec::Generic => State::Generic as i32,
                    GroupCallSpec::Joined => State::Joined as i32,
                    GroupCallSpec::Ringing => State::Ringing as i32,
                    GroupCallSpec::Accepted => State::Accepted as i32,
                    GroupCallSpec::Declined => State::Declined as i32,
                    GroupCallSpec::Missed => State::Missed as i32,
                    GroupCallSpec::MissedNotificationProfile => {
                        State::MissedNotificationProfile as i32
                    }
                    GroupCallSpec::OutgoingRing => State::OutgoingRing as i32,
                    GroupCallSpec::Number(n) => *n,
                },
                ringer_recipient_id: ringer.map(who),
                started_call_recipient_id: started_by.map(who),
                started_call_timestamp: *started_at,
                ended_call_timestamp: None,
                read: *read,
            }))
        }
    }
}

/// The proto's number of a [`SimpleSpec`].
fn simple_number(kind: SimpleSpec) -> i32 {
    use backup::simple_chat_update::Type;
    match kind {
        SimpleSpec::JoinedSignal => Type::JoinedSignal as i32,
        SimpleSpec::IdentityUpdate => Type::IdentityUpdate as i32,
        SimpleSpec::IdentityVerified => Type::IdentityVerified as i32,
        SimpleSpec::IdentityDefault => Type::IdentityDefault as i32,
        SimpleSpec::ChangeNumber => Type::ChangeNumber as i32,
        SimpleSpec::ReleaseChannelDonationRequest => Type::ReleaseChannelDonationRequest as i32,
        SimpleSpec::EndSession => Type::EndSession as i32,
        SimpleSpec::ChatSessionRefresh => Type::ChatSessionRefresh as i32,
        SimpleSpec::BadDecrypt => Type::BadDecrypt as i32,
        SimpleSpec::PaymentsActivated => Type::PaymentsActivated as i32,
        SimpleSpec::PaymentActivationRequest => Type::PaymentActivationRequest as i32,
        SimpleSpec::UnsupportedProtocolMessage => Type::UnsupportedProtocolMessage as i32,
        SimpleSpec::ReportedSpam => Type::ReportedSpam as i32,
        SimpleSpec::Blocked => Type::Blocked as i32,
        SimpleSpec::Unblocked => Type::Unblocked as i32,
        SimpleSpec::MessageRequestAccepted => Type::MessageRequestAccepted as i32,
        SimpleSpec::Number(n) => n,
    }
}

/// Writes the history `spec` as an archive at `path`, encrypted for a device that was
/// linked with `ephemeral_key` as account `own_aci`. Returns how many frames a reader gets
/// from it when it is whole (the header is not counted).
pub(crate) fn write_history(
    path: &Path,
    ephemeral_key: &[u8; 32],
    own_aci: Aci,
    spec: &HistorySpec,
) -> io::Result<u64> {
    let key = ArchiveKey::for_transfer(ephemeral_key, own_aci);
    let mut info = sample_info();
    info.version = spec.version;
    if let Some(export_time_ms) = spec.export_time_ms {
        info.backup_time_ms = export_time_ms;
    }
    let mut writer = ArchiveWriter::new(&info);
    let mut spec = spec.clone();
    spec.own_aci.get_or_insert(uuid::Uuid::from(own_aci));
    let frames = history_frames(&spec);
    for (i, frame) in frames.iter().enumerate() {
        if spec.broken_after == Some(u32::try_from(i).unwrap_or(u32::MAX)) {
            // A length-delimited field that claims more bytes than the frame has.
            writer.raw_frame(&[0x0a, 0x7f, 0x01]);
        }
        writer.frame(frame);
    }
    writer.write(path, &key)?;
    Ok(frames.len() as u64)
}
