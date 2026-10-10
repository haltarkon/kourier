//! Kourier's data contract: ids, content model, features, receipts and errors. Pure serde data.
//!
//! Everything here crosses the core ↔ backend boundary or is persisted by the core
//! (docs/architecture.md §3, §4). Persisted shapes follow two evolution rules: enums are
//! serialized by variant *name* (never by index), and fields that may be absent in older data
//! carry `#[serde(default)]`.
//!
//! Log safety (§9.1): the `Debug` output of every type here is log-safe. Ids print in short
//! form ([`UserId::log_safe`], [`ConvKey::log_safe`]); message texts, names, titles, file names,
//! phone numbers, keys, tokens and QR contents print as `[redacted]` or only their shape (see
//! [`log`]). Remaining strings in `Debug` (error details, reasons, kind hints) are log-safe by
//! contract. Accessors such as [`UserId::as_str`] return the real values: never log those.

pub mod account;
pub mod call;
pub mod conn;
pub mod contact;
pub mod content;
pub mod conversation;
pub mod error;
pub mod features;
pub mod ids;
pub mod log;
pub mod login;
pub mod patch;
pub mod state;
pub mod text;
pub mod transfer;

mod serde_util;

pub use account::{
    Account, AccountLocalPrefs, AccountSettings, AccountSettingsPatch, AccountState,
    AutoDownloadPolicy, OwnDevice, ProfilePatch,
};
pub use call::{CallDirection, CallLine, CallMedia, CallState, MAX_RING_MS};
pub use conn::{ConnState, UserAction};
pub use contact::{
    Contact, ContactChange, ContactPatch, IdentityFingerprint, Member, MemberList, MemberRole,
    TrustLevel,
};
pub use content::{
    Attachment, AttachmentKind, AttachmentMeta, AttachmentState, BlobKey, BlobRef, Body,
    ContactCard, Deletion, DisappearKind, Expiry, Location, Message, Origin, Placeholder, Poll,
    QuoteSnapshot, ReplyTo, SystemEvent,
};
pub use conversation::{ConvKind, Conversation, ConversationChange, ConversationPatch};
pub use error::{BackendError, ErrorKind, PersistError};
pub use features::{
    AccountCaps, CannotSend, ConversationFeatures, FileAlbum, FileLimits, FilePacking, FilePlan,
    FormatKind, GroupActions, HistorySource, LoginFlow, NetworkDescriptor, PlanFile, RiskNotice,
    Support, TimerRange, TypingPolicy, Window,
};
pub use ids::{
    AccountId, AccountUuid, AttachmentId, BackendBlob, CmdId, ContactId, ConvKey, ConversationId,
    DownloadId, IdError, Identifier, IdentifierKind, LocalMsgId, MessageId, MsgIdScope, MsgTarget,
    NetMsgId, NetworkKind, TargetAuthor, UserId,
};
pub use log::{Redacted, RedactedShape};
pub use login::{InputField, InputKind, LoginStep, LoginStepKind};
pub use patch::Patch;
pub use state::{FailReason, OutgoingState, ReadState, ReceiptKind, RecipientState};
pub use text::{RangeError, RangeKind, Text, TextRange, Utf16Map};
pub use transfer::{HistoryTransfer, HistoryTransferFailure};

/// FNV-1a 64-bit hash of `bytes`: stable across builds, platforms and runs (unlike
/// `std::hash`), so it can be persisted (`features_hash`) and used for log tags. Not
/// cryptographic.
pub fn stable_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::stable_hash;

    #[test]
    fn stable_hash_matches_fnv1a_vectors() {
        assert_eq!(stable_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(stable_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(stable_hash(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
