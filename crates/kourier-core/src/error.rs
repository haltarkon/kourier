//! The core's error type.

use kourier_backend_api::ContractViolation;
use kourier_model::{AccountId, BackendError, ConversationId, NetworkKind};

use crate::vault::VaultError;

/// Errors of the core. Every variant is log-safe (no message content, names or keys).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Opening, migrating or writing `core.db` failed.
    #[error(transparent)]
    Db(#[from] kourier_db::Error),
    /// A query failed.
    #[error("core.db: {0}")]
    Sql(#[from] kourier_db::rusqlite::Error),
    /// A stored JSON column could not be encoded or decoded.
    #[error("core.db JSON column: {0}")]
    Json(String),
    /// The account vault (key.wrap and account secrets) failed.
    #[error(transparent)]
    Vault(#[from] VaultError),
    /// A backend refused a request (boxed: it is by far the largest thing an error of the core
    /// carries, and errors travel by value).
    #[error(transparent)]
    Backend(Box<BackendError>),
    /// A backend broke the contract (§10.2).
    #[error(transparent)]
    Contract(#[from] ContractViolation),
    /// No network of this kind is compiled in.
    #[error("network {0} is not available in this build")]
    UnknownNetwork(NetworkKind),
    /// No such account.
    #[error("unknown account {0}")]
    UnknownAccount(AccountId),
    /// No such conversation.
    #[error("unknown conversation {0}")]
    UnknownConversation(ConversationId),
    /// The request does not fit the account's state (for example relinking an active account).
    #[error("invalid account state: {0}")]
    InvalidState(&'static str),
    /// The request itself is invalid (no files, an unknown attachment); a caller's mistake
    /// more than something to tell the user. What a user action can run into is
    /// [`Refused`](Self::Refused).
    #[error("invalid request: {0}")]
    InvalidInput(&'static str),
    /// The core will not do what the user asked, for a reason a UI can name: `reason` is
    /// what to tell apart, `detail` the log-safe sentence for the log (it never names
    /// content). [`Self::refusal`] reads the reason.
    #[error("invalid request: {detail}")]
    Refused {
        /// Why, for the UI.
        reason: Refusal,
        /// Why, for the log.
        detail: &'static str,
    },
    /// The blob store (encrypted attachment files) failed.
    #[error("attachment file: {0}")]
    Blob(#[from] kourier_blob::BlobError),
    /// A link brought an identity that another account of this profile already has (the
    /// same primary own id, on the same network): two devices of one account in one profile
    /// would show every chat twice. `logged_out`: that account is logged out.
    #[error("this account is already in the profile")]
    AlreadyAdded {
        /// The existing account is logged out ("Link Again" on it is the way back).
        logged_out: bool,
    },
    /// The core is shutting down or has stopped.
    #[error("the core has stopped")]
    Stopped,

    /// The command exists in the facade but its step is not implemented yet.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
}

impl From<BackendError> for CoreError {
    fn from(e: BackendError) -> Self {
        Self::Backend(Box::new(e))
    }
}

/// Why the core refused a user action ([`CoreError::Refused`]): the reasons a UI words
/// differently. Which action was refused is the event's `action`, so "too old" is "too old to
/// edit" for `EditMessage` and "too old to delete for everyone" for `DeleteMessage`. The
/// checks that produce them are the ones behind `MessageActions`, so a refusal reaches the UI
/// only when the facts changed between the row it showed and the command (or for what the
/// flags cannot know: the command's own input).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Refusal {
    /// The message is older than the chat's window for this action (edit, delete for
    /// everyone).
    TooOld,
    /// The message was edited as often as the chat allows.
    EditedTooOften,
    /// The message (or the message to quote) is deleted: a tombstone is all that is left.
    Deleted,
    /// The message's own send is on its way or waits in the outbox: the action has to wait
    /// for the answer (a delete while the transmit is in flight, deleting an attachment of a
    /// message that is not sent yet).
    BeingSent,
    /// Nothing would change: the new text is the text the message has.
    Unchanged,
    /// The conversation does not take this (its features): no edits, no delete for
    /// everyone, no reactions, no replies, no caption with this kind of file, no quote on a
    /// message with files and no text, a disappearing timer it does not offer.
    NotAllowedHere,
    /// Only the author may do this, and that is someone else (or, for a delete for
    /// everyone, we are no admin of the group).
    NotOwn,
    /// This kind of message does not take the action: a notice cannot be edited, quoted,
    /// reacted to or deleted for everyone, a sticker has no text to edit.
    WrongKind,
    /// There would be nothing left: an empty text, an empty message.
    Empty,
    /// The text is longer than one message of the chat carries, and this action cannot
    /// split it (an edit).
    TooLong,
    /// The chat's limit for this is reached (reactions of one user on a message).
    TooMany,
    /// The conversation is blocked.
    Blocked,
    /// We cannot send in this conversation (not a member, admins only, …:
    /// `ConversationFeatures::cannot_send`).
    CannotSend,
    /// The message, the attachment or the quoted message is not there (any more): deleted
    /// for good, expired, or never in this conversation.
    Gone,
}

impl CoreError {
    /// A refusal a UI can name.
    pub(crate) fn refused(reason: Refusal, detail: &'static str) -> Self {
        Self::Refused { reason, detail }
    }

    /// Why the core refused the action, when it did so for a reason a UI can name.
    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            Self::Refused { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    /// A JSON error, keeping only serde's category and position (never the input, §9.1).
    pub(crate) fn json(e: &serde_json::Error) -> Self {
        Self::Json(format!(
            "{:?} error at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ))
    }
}

/// `Result` with [`CoreError`].
pub type Result<T, E = CoreError> = std::result::Result<T, E>;
