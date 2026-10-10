//! The history-transfer archive: the file the phone uploads when a device is linked with
//! `backup5` (docs/architecture.md §14.2 item 14). This module turns the downloaded FILE into
//! typed frames, safely. It has no network code and does not touch the core's model; the
//! link-time protocol that downloads the file and drives this reader is `transfer.rs`.
//!
//! # The format
//!
//! `IV (16) ‖ AES-256-CBC(gzip(frames)) ‖ HMAC-SHA256(IV ‖ ciphertext)`, where `frames` is one
//! `BackupInfo` and then any number of `Frame`s, each as a varint length and a protobuf
//! message (`protos/backup.proto`, libsignal v0.104.0). The keys are `ArchiveKey`: the 32
//! bytes the phone put in the provisioning message as its ephemeral backup key, used as a
//! libsignal `BackupKey`, with the backup id derived from our ACI and no forward-secrecy
//! token (what the phone writes with `createForLocalOrLinking` and Desktop reads). A file
//! that starts with libsignal's magic number (the format of remote backups, with an
//! unencrypted metadata header that no MAC covers) is REFUSED before anything of it is
//! parsed: a transfer archive never has that header, libsignal parses it before any check
//! with a reader that allocates by a length it reads, and a header put in front of a real
//! archive would be accepted. The phone appends zero padding after the gzip stream
//! (`PaddedGzipOutputStream`); the decoder stops at the end of the first gzip member, and
//! what follows it is covered by the MAC and otherwise ignored (as in libsignal's reader).
//!
//! # How it is read
//!
//! libsignal's `frame::FramesReader` does the cryptography: one pass over the whole file for
//! the HMAC (before anything is decrypted), then AES-256-CBC and gzip as a stream. Its typed
//! model of a backup is closed, so frames are decoded here with our own prost types, and the
//! varint framing is ours too, because libsignal's `VarintDelimitedReader` allocates by the
//! length it reads. What the format itself does not bound, [`Limits`] does:
//!
//! - one frame's length is checked BEFORE a byte is allocated for it;
//! - one frame's fields are counted before it is decoded (its bytes do not bound what it
//!   decodes to: two bytes in the file can be a struct of 800 bytes in memory);
//! - the total decompressed bytes (a gzip bomb stops at the cap, never at the end of the file);
//! - the number of frames.
//!
//! The file is never held in memory: the reader keeps one frame at a time, in a buffer that
//! is wiped when it goes.
//!
//! # Frames are not trusted until the end
//!
//! The first pass authenticates the file; the frames come from a second pass over the same
//! path, and the MAC of what that pass decrypted is checked only when it ends. A file that
//! changed in between yields frames no MAC covered, and the error comes with the last call.
//! So: the downloader writes the archive into a directory only this user can write, and the
//! importer treats what it stored as provisional until `next_frame` returned `Ok(None)`: on
//! any error it removes what it imported from this archive.
//!
//! # How it is meant to be called
//!
//! The reader does blocking file I/O and a lot of CPU (the HMAC pass over the whole file in
//! [`ArchiveReader::open`], then decryption and decompression as frames are read). Call it from
//! `tokio::task::spawn_blocking`, never from the account actor's thread. An [`ArchiveReader`] is
//! not `Send` (libsignal's AES reader shares its source through an `Rc`): open it and drain it
//! inside the one blocking closure, and send the frames (which are `Send`) or the import's
//! result out of it:
//!
//! ```ignore
//! tokio::task::spawn_blocking(move || -> Result<Counts, ArchiveError> {
//!     let mut reader = ArchiveReader::open(&path, &key, Limits::default())?;
//!     while let Some(frame) = reader.next_frame()? {
//!         // map the frame; Frame::Unknown is skipped, counted
//!     }
//!     Ok(counts)
//! })
//! ```
//!
//! `next_frame` returns `Ok(None)` only after the end of the stream AND a second HMAC check of
//! the bytes that were decrypted (a file that changed between the two passes is an error, not a
//! short archive). An error ends the reader: every later call returns the same error.
//!
//! # Where this differs from libsignal's validator
//!
//! libsignal's `BackupReader` is a validator: it fails the whole read on the first invalid
//! frame, and on a frame whose `item` oneof is unset or of a kind it does not know. This
//! reader does not: `backup.proto` says importers should skip such a frame, and a phone newer
//! than this Kourier will send frame and item kinds it has never seen, so those frames come
//! out as [`Frame::Unknown`] and the import goes on. (A frame that does not decode at all is
//! still an error, and so is a `BackupInfo` of a version this reader does not know.) This
//! reader also checks none of the ordering and cross-reference rules the validator checks
//! (a chat for a recipient that never came, two self recipients): those are the importer's
//! to meet, frame by frame.
//!
//! # Secrets
//!
//! The archive holds the account's profile key and SVR PIN (`AccountData`), message bodies and
//! the media root key. Nothing here logs, or puts into an error, any of it: the `Debug` of a
//! [`Frame`], an [`ArchiveInfo`] and an [`ArchiveKey`] print a kind or nothing, and an
//! [`ArchiveError`] says only what went wrong with the file, never what was in it. The
//! prost types inside a [`Frame`] derive `Debug` in full: never format one.

mod key;
mod reader;
#[cfg(any(test, feature = "testing"))]
pub(crate) mod writer;

#[cfg(test)]
mod tests;

use std::fmt;
use std::io;

use zeroize::Zeroizing;

use crate::proto::backup;

pub(crate) use key::ArchiveKey;
pub(crate) use reader::ArchiveReader;

/// The only `BackupInfo.version` this reader knows. Signal Desktop refuses any other value
/// the same way (`BACKUP_VERSION`, `ts/services/backups/constants.std.ts`), and the Android
/// phone writes 1 (`BackupRepository.VERSION`).
pub(crate) const SUPPORTED_VERSION: u64 = 1;

/// What the reader enforces that the format does not (architecture §13 phase H lists every
/// limit with its enforcer; these three are new rows).
///
/// The archive comes from the user's own phone and is authenticated by a key only that phone
/// and this device hold, so the threat is a bug, a newer phone or a hostile key holder, not an
/// attacker on the network. The limits keep any of those from costing more memory, disk or time
/// than a real account can need; they are not meant to bound the size of a real account, and
/// none of them comes from a measured archive (there is none yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The longest encoded frame, in bytes. Checked on the length prefix, before anything is
    /// allocated. The validator caps a message body at 128 KiB (`text.rs`); a chat item also
    /// carries its edit revisions (each a whole item, Signal allows ten edits), reactions and
    /// attachment pointers: about 1.5 MiB at the very most. 4 MiB is well above that and far
    /// below the 4 GiB a length prefix can claim.
    pub(crate) max_frame_bytes: u32,
    /// The most fields in one frame, counted through every nested message. The bytes of a
    /// frame do not bound what it decodes to (an empty element of a repeated message is two
    /// bytes in the file and a struct of some 800 bytes in memory), the fields do: 100,000 of
    /// them are at most some 80 MB while one frame is decoded. A chat item with a thousand
    /// reactions and ten revisions, or a group of a thousand members, has a few thousand.
    pub(crate) max_frame_fields: u64,
    /// The most bytes the gzip stream may produce in all (frame prefixes included). Gzip can
    /// turn a few KiB into GiB, so this is what stops a bomb. By an estimate of about 300 bytes
    /// a text message, 8 GiB is some 25 million messages: far more than an account has.
    pub(crate) max_decompressed_bytes: u64,
    /// The most frames after the `BackupInfo`. An empty frame is one byte, so without this a
    /// stream of zeros inside the byte cap would be billions of frames. 20 million is four
    /// times the frames of an account with five million messages.
    pub(crate) max_frames: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 4 * 1024 * 1024,
            max_frame_fields: 100_000,
            max_decompressed_bytes: 8 * 1024 * 1024 * 1024,
            max_frames: 20_000_000,
        }
    }
}

/// The archive's first frame, `BackupInfo`, in plain data. `Debug` prints nothing secret.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ArchiveInfo {
    /// The format version; [`ArchiveReader::open`] accepts only [`SUPPORTED_VERSION`].
    pub(crate) version: u64,
    /// When the phone made the archive (Unix milliseconds).
    pub(crate) backup_time_ms: u64,
    /// The phone's app version when it made the archive.
    pub(crate) current_app_version: String,
    /// The app version the account first registered on.
    pub(crate) first_app_version: String,
    /// The media root backup key the phone put in the archive, when it is 32 bytes.
    pub(crate) media_root_backup_key: Option<Zeroizing<[u8; 32]>>,
}

impl ArchiveInfo {
    /// From the decoded frame; `debug_info` (a client's free-form testing data) is dropped.
    fn from_proto(mut info: backup::BackupInfo) -> Self {
        let key = Zeroizing::new(std::mem::take(&mut info.media_root_backup_key));
        Self {
            version: info.version,
            backup_time_ms: info.backup_time_ms,
            current_app_version: std::mem::take(&mut info.current_app_version),
            first_app_version: std::mem::take(&mut info.first_app_version),
            media_root_backup_key: <[u8; 32]>::try_from(key.as_slice())
                .ok()
                .map(Zeroizing::new),
        }
    }

    /// The frame as it goes on the wire (the writer's, and a test's).
    #[cfg(any(test, feature = "testing"))]
    fn to_proto(&self) -> backup::BackupInfo {
        backup::BackupInfo {
            version: self.version,
            backup_time_ms: self.backup_time_ms,
            media_root_backup_key: self
                .media_root_backup_key
                .as_deref()
                .map(|key| key.to_vec())
                .unwrap_or_default(),
            current_app_version: self.current_app_version.clone(),
            first_app_version: self.first_app_version.clone(),
            debug_info: Vec::new(),
        }
    }
}

impl fmt::Debug for ArchiveInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchiveInfo")
            .field("version", &self.version)
            .field("backup_time_ms", &self.backup_time_ms)
            .finish_non_exhaustive()
    }
}

/// One frame of the archive, by kind.
///
/// A frame whose `item` is unset, or of a kind this version of `backup.proto` does not have,
/// is [`Frame::Unknown`] and NOT an error (see the module docs). `Debug` prints the kind only;
/// the payloads are prost messages that print everything, so never format them.
#[derive(Clone, PartialEq)]
pub(crate) enum Frame {
    /// The account's own data: profile key, settings, the SVR PIN. Always the first frame.
    Account(Box<backup::AccountData>),
    /// A person, group, story list, the note-to-self or Signal's release notes.
    Recipient(backup::Recipient),
    /// A conversation with one recipient.
    Chat(backup::Chat),
    /// A message or a notice in a chat.
    ChatItem(Box<backup::ChatItem>),
    /// A sticker pack the user installed.
    InstalledStickerPack(backup::StickerPack),
    /// A sticker pack the user once had.
    HiddenStickerPack(backup::StickerPack),
    /// A call made from a call link.
    AdHocCall(backup::AdHocCall),
    /// A notification profile.
    NotificationProfile(backup::NotificationProfile),
    /// A chat folder.
    ChatFolder(backup::ChatFolder),
    /// The favourite stickers.
    FavoriteStickerList(backup::FavoriteStickerList),
    /// An unset `item`, or one of a kind this reader does not know. Skip it, and count it.
    Unknown,
}

impl Frame {
    /// The kind, for logs and tests (a name, never content).
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Account(_) => "account",
            Self::Recipient(_) => "recipient",
            Self::Chat(_) => "chat",
            Self::ChatItem(_) => "chat_item",
            Self::InstalledStickerPack(_) => "installed_sticker_pack",
            Self::HiddenStickerPack(_) => "hidden_sticker_pack",
            Self::AdHocCall(_) => "ad_hoc_call",
            Self::NotificationProfile(_) => "notification_profile",
            Self::ChatFolder(_) => "chat_folder",
            Self::FavoriteStickerList(_) => "favorite_sticker_list",
            Self::Unknown => "unknown",
        }
    }

    fn from_proto(frame: backup::Frame) -> Self {
        use backup::frame::Item;
        match frame.item {
            Some(Item::Account(v)) => Self::Account(Box::new(v)),
            Some(Item::Recipient(v)) => Self::Recipient(v),
            Some(Item::Chat(v)) => Self::Chat(v),
            Some(Item::ChatItem(v)) => Self::ChatItem(Box::new(v)),
            Some(Item::InstalledStickerPack(v)) => Self::InstalledStickerPack(v),
            Some(Item::HiddenStickerPack(v)) => Self::HiddenStickerPack(v),
            Some(Item::AdHocCall(v)) => Self::AdHocCall(v),
            Some(Item::NotificationProfile(v)) => Self::NotificationProfile(v),
            Some(Item::ChatFolder(v)) => Self::ChatFolder(v),
            Some(Item::FavoriteStickerList(v)) => Self::FavoriteStickerList(v),
            None => Self::Unknown,
        }
    }

    /// The frame as it goes on the wire (the writer's).
    #[cfg(any(test, feature = "testing"))]
    fn to_proto(&self) -> backup::Frame {
        use backup::frame::Item;
        let item = match self.clone() {
            Self::Account(v) => Some(Item::Account(*v)),
            Self::Recipient(v) => Some(Item::Recipient(v)),
            Self::Chat(v) => Some(Item::Chat(v)),
            Self::ChatItem(v) => Some(Item::ChatItem(*v)),
            Self::InstalledStickerPack(v) => Some(Item::InstalledStickerPack(v)),
            Self::HiddenStickerPack(v) => Some(Item::HiddenStickerPack(v)),
            Self::AdHocCall(v) => Some(Item::AdHocCall(v)),
            Self::NotificationProfile(v) => Some(Item::NotificationProfile(v)),
            Self::ChatFolder(v) => Some(Item::ChatFolder(v)),
            Self::FavoriteStickerList(v) => Some(Item::FavoriteStickerList(v)),
            Self::Unknown => None,
        };
        backup::Frame { item }
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Frame::{}", self.kind())
    }
}

/// Why an archive could not be read. Says what is wrong with the FILE, never what was in it;
/// each kind is a different thing for the user to do (retry the download, update Kourier, link
/// again without a transfer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ArchiveError {
    /// The file could not be opened or read (the operating system's reason, as a kind).
    #[error("the archive file could not be read ({0:?})")]
    Io(io::ErrorKind),
    /// The HMAC does not match: a wrong key, a corrupted download, a changed file, or a file
    /// cut anywhere but in its first 40 bytes (the MAC is the last 32 bytes of the file, so a
    /// cut file fails its MAC like a flipped byte; compare the length with the download's).
    #[error("the archive failed its integrity check")]
    Hmac,
    /// The file or the decrypted stream ended too early: a file shorter than the smallest
    /// archive, or, with a valid MAC, a stream that stops inside a frame or inside its gzip.
    #[error("the archive is cut short")]
    Truncated,
    /// The file begins with the magic number of the backup format that has an unencrypted
    /// header (remote backups). A transfer archive never has one, and it is refused before
    /// anything of it is parsed.
    #[error("the file is not a transfer archive (it has the header of another backup format)")]
    BadHeader,
    /// The AES-CBC padding of the last block is invalid (with a valid MAC).
    #[error("the archive's padding is invalid")]
    BadPadding,
    /// The decrypted bytes are not valid gzip (with a valid MAC).
    #[error("the archive is not valid gzip")]
    BadGzip,
    /// A frame's length prefix is not a 32-bit varint.
    #[error("the archive's framing is invalid")]
    BadFraming,
    /// A frame does not decode as protobuf.
    #[error("a frame of the archive does not decode")]
    BadProtobuf,
    /// The archive has no `BackupInfo`: the stream is empty.
    #[error("the archive has no header frame")]
    MissingInfo,
    /// `BackupInfo.version` is not [`SUPPORTED_VERSION`]: the user needs a newer Kourier (or the
    /// archive is not a backup). Carries the version, which is not content.
    #[error("the archive's format version {found} is not supported")]
    UnsupportedVersion {
        /// The version the archive states.
        found: u64,
    },
    /// A frame claims more than [`Limits::max_frame_bytes`]. Carries the claimed length.
    #[error("a frame of {len} bytes is longer than the limit of {max}")]
    FrameTooLong {
        /// The length the frame prefix claims.
        len: u64,
        /// The limit.
        max: u64,
    },
    /// The stream decompressed to more than [`Limits::max_decompressed_bytes`].
    #[error("the archive decompresses to more than {max} bytes")]
    TooLarge {
        /// The limit.
        max: u64,
    },
    /// More than [`Limits::max_frames`] frames.
    #[error("the archive holds more than {max} frames")]
    TooManyFrames {
        /// The limit.
        max: u64,
    },
    /// A frame with more than [`Limits::max_frame_fields`] fields, or nested deeper than a
    /// decoder goes: it would decode to far more memory than its bytes.
    #[error("a frame of the archive has more than {max} fields")]
    FrameTooComplex {
        /// The limit.
        max: u64,
    },
}
