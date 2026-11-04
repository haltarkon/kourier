//! Errors of the Telegram backend. A failure carries the NAME of the operation or a fixed class and
//! nothing else: no phone number, user id, code, message text, key, TDLib JSON or TDLib error text
//! (architecture §9.1; decisions D3 M5). The SQLite error itself is logged once, where it is mapped,
//! with the operation and the result code only.

use kourier_db::rusqlite;

/// A failure of the account's store: the name of the operation and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct StoreError(pub(crate) &'static str);

/// Maps a rusqlite failure of `op` to a [`StoreError`] and logs it once: only the operation and the
/// SQLite (extended) result code (SQLite's text can name a table, a column or a constraint).
pub(crate) fn sql(op: &'static str) -> impl FnOnce(rusqlite::Error) -> StoreError {
    move |e| {
        let code = e.sqlite_error().map(|e| e.extended_code);
        tracing::error!(op, code, "telegram backend.db store failure");
        StoreError(op)
    }
}

/// Why the account's `backend.db` did not open. The file is not touched by a wrong key or a
/// foreign file.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The key does not decrypt the file, or the file is not a Kourier database. Nothing was written.
    #[error("wrong key, or not a Kourier database")]
    WrongKey,
    /// The file was written by a newer Kourier: never opened, never downgraded.
    #[error(
        "the {component} schema is version {found}, newer than this build supports ({supported}); update Kourier"
    )]
    TooNew {
        /// The schema component (`telegram` or `core_outbox`).
        component: String,
        /// The version in the file.
        found: i64,
        /// The latest version this binary knows.
        supported: i64,
    },
    /// The file carries a schema component that is not ours (another network's backend, the core):
    /// refused before anything was written.
    #[error("this backend.db belongs to another network")]
    OtherNetwork,
    /// Any other failure of the database layer (the migration, the pragmas).
    #[error("the account database could not be opened: {0}")]
    Failed(String),
}

impl From<kourier_db::Error> for OpenError {
    fn from(e: kourier_db::Error) -> Self {
        match e {
            kourier_db::Error::WrongKey => Self::WrongKey,
            kourier_db::Error::ForeignComponent { .. } => Self::OtherNetwork,
            kourier_db::Error::TooNew {
                component,
                found,
                supported,
            } => Self::TooNew {
                component,
                found,
                supported,
            },
            // The text of kourier-db's errors names components, versions and steps, never a key or
            // a path of data.
            other => Self::Failed(other.to_string()),
        }
    }
}

/// Why the link to a helper failed. Fixed classes only: never a frame's content, a TDLib JSON
/// line or TDLib's error message (decisions D3 M5); a TDLib error keeps its numeric code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum LinkError {
    /// No hello arrived in time.
    #[error("the TDLib helper sent no hello in time")]
    HelloTimeout,
    /// The helper speaks another protocol or serves another TDLib than the pinned one.
    #[error("the TDLib helper is not the pinned build")]
    VersionMismatch,
    /// The helper's end of the link is gone (or the link was closed).
    #[error("the TDLib helper's link is closed")]
    Closed,
    /// The helper wrote something that is not the frame expected here.
    #[error("the TDLib helper wrote an unexpected frame")]
    Frame,
    /// A request got no answer in time (its late answer is counted as a stray).
    #[error("a TDLib request got no answer in time")]
    Timeout,
    /// The helper refused the request (a fixed code of the frame protocol).
    #[error("the TDLib helper refused a request ({0})")]
    Refused(kourier_tdlib::protocol::FailureCode),
    /// TDLib answered with an error; only its code is kept.
    #[error("TDLib answered with error {code}")]
    Td {
        /// TDLib's numeric error code.
        code: i32,
    },
    /// TDLib could not lock its database: another helper (an orphan inside its own deadline) still
    /// holds the account's `tdlib/` directory (critic L8; `FileFd.cpp:504`, BSL-1.0, read for
    /// behaviour). A fixed class, found by a test on TDLib's message that is never kept or logged.
    #[error("TDLib's database is in use by another helper")]
    DatabaseLocked,
    /// TDLib answered with an error of a class the login or the session acts on ([`TdClass`],
    /// TG-2 brief §2.8): the code, the fixed class and, for a 429, the seconds to wait. The
    /// message itself was looked at for the class and dropped.
    #[error(
        "TDLib answered with error {code} of a known class ({class:?}, wait {retry_after_s:?} s)"
    )]
    Auth {
        /// TDLib's numeric error code.
        code: i32,
        class: TdClass,
        /// A 429's "retry after N" (seconds), when it names one.
        retry_after_s: Option<u32>,
    },
}

/// The error classes of TDLib's answers the login machine and the session act on (TG-2 brief
/// §2.8; critic B1 splits them into terminal and retryable in the machine). Found by EXACT
/// equality with TDLib's message (two prefix forms: the 429's wait and TDLib's "Call to <request>
/// unexpected"); the message is never kept or logged. Anything else is a plain `LinkError::Td`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TdClass {
    /// `PHONE_NUMBER_INVALID`.
    PhoneNumberInvalid,
    /// `PHONE_NUMBER_BANNED`.
    PhoneNumberBanned,
    /// `PHONE_NUMBER_FLOOD`: too many codes asked for this number.
    PhoneNumberFlood,
    /// `PHONE_CODE_INVALID`.
    PhoneCodeInvalid,
    /// `PHONE_CODE_EXPIRED`.
    PhoneCodeExpired,
    /// `PHONE_CODE_EMPTY`.
    PhoneCodeEmpty,
    /// `PASSWORD_HASH_INVALID`: a wrong 2FA password.
    PasswordHashInvalid,
    /// `EMAIL_INVALID`.
    EmailInvalid,
    /// `EMAIL_CODE_INVALID`.
    EmailCodeInvalid,
    /// `EMAIL_NOT_ALLOWED`.
    EmailNotAllowed,
    /// `CODE_INVALID` (TDLib's own, `PasswordManager.cpp:433`).
    CodeInvalid,
    /// `FROZEN_METHOD_INVALID` (406): a frozen account may not do this.
    FrozenMethodInvalid,
    /// TDLib's `Invalid recovery code`.
    InvalidRecoveryCode,
    /// TDLib's `Application update is needed to log in` (an unknown password KDF).
    UpdateNeeded,
    /// TDLib's `Another authorization query has started` (a second auth request in flight).
    AnotherQuery,
    /// TDLib's `Call to <request> unexpected` (a request in the wrong state).
    Unexpected,
    /// 429 `Too Many Requests: retry after N` (TDLib's flood wait past the request's limit).
    TooManyRequests,
}
