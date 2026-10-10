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
/// (ids from 2, in the order of [`contacts`](Self::contacts)), the release notes' recipient
/// when asked for, one chat per entry of [`chats`](Self::chats) (ids from 1), the release
/// notes' chat, then the items: chat after chat, or interleaved item by item, then the
/// orphans and the generated messages.
#[derive(Clone, Debug)]
pub struct HistorySpec {
    /// The people the phone knows. One that no chat names is a recipient without a chat.
    pub contacts: Vec<ContactSpec>,
    /// The chats. One without a text is a chat without an item.
    pub chats: Vec<ChatSpec>,
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
}

impl Default for HistorySpec {
    fn default() -> Self {
        Self {
            contacts: Vec::new(),
            chats: Vec::new(),
            interleaved: false,
            release_notes_items: None,
            orphan_items: 0,
            generated: None,
            version: SUPPORTED_VERSION,
            broken_after: None,
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
}

/// A chat of a [`HistorySpec`].
#[derive(Clone, Debug)]
pub struct ChatSpec {
    /// Whom it is with.
    pub with: ChatWith,
    /// Its text messages, in the order the phone received them.
    pub texts: Vec<TextSpec>,
}

/// Whom a chat of a [`HistorySpec`] is with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatWith {
    /// The contact at this index of [`HistorySpec::contacts`].
    Contact(usize),
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
    use backup::chat_item::{
        DirectionalDetails, IncomingMessageDetails, Item, OutgoingMessageDetails,
    };
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
    // `peer` is the recipient id of the chat's other side (ourselves in Note to Self).
    let text = |chat_id: u64, peer: u64, t: &TextSpec| {
        let details = if t.outgoing {
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
        Frame::ChatItem(Box::new(backup::ChatItem {
            chat_id,
            author_id: match (t.outgoing, t.unknown_author) {
                (true, _) => SELF_ID,
                (false, true) => NOBODY,
                (false, false) => peer,
            },
            date_sent: t.sent_at,
            expire_start_date: t.expire_start,
            expires_in_ms: t.expires_in_ms,
            directional_details: Some(details),
            item: Some(Item::StandardMessage(backup::StandardMessage {
                text: Some(backup::Text {
                    body: t.body.clone(),
                    body_ranges: Vec::new(),
                }),
                ..Default::default()
            })),
            ..Default::default()
        }))
    };

    let mut frames = vec![
        Frame::Account(Box::new(backup::AccountData {
            profile_key: vec![1; 32],
            given_name: "Test".to_owned(),
            ..Default::default()
        })),
        recipient(SELF_ID, Destination::Self_(backup::Self_::default())),
    ];
    let contact_id = |index: usize| 2 + index as u64;
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

    let chat = |id, recipient_id| {
        Frame::Chat(backup::Chat {
            id,
            recipient_id,
            ..Default::default()
        })
    };
    let peer_of = |with: ChatWith| match with {
        ChatWith::Contact(index) => contact_id(index),
        ChatWith::NoteToSelf => SELF_ID,
        ChatWith::Nobody => NOBODY,
    };
    let chat_id = |index: usize| 1 + index as u64;
    for (index, c) in spec.chats.iter().enumerate() {
        frames.push(chat(chat_id(index), peer_of(c.with)));
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
    let mut writer = ArchiveWriter::new(&info);
    let frames = history_frames(spec);
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
