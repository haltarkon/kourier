//! Errors of the store. A failure carries the NAME of the operation and nothing else: no JID, address,
//! phone number, message id, token, key or SQL text (architecture §9.1). The SQLite error itself is
//! logged once, at the place that maps it, with the operation and the result code only.

use kourier_db::rusqlite;
use wacore::store::error::StoreError;

/// The message of a store failure: an operation name such as "put session failed". It is the
/// source of `StoreError::Database`, so `Display` of the whole chain prints it and no input.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Failed(&'static str);

/// A database failure of `what` (a short fixed sentence naming the operation).
pub(crate) fn database(what: &'static str) -> StoreError {
    StoreError::Database(Box::new(Failed(what)))
}

/// A stored row that cannot be read back as what it should be (a damaged or foreign record). `what`
/// names the record kind, never its content.
pub(crate) fn unreadable(what: &'static str) -> StoreError {
    StoreError::Serialization(Box::new(Failed(what)))
}

/// A refused input. `what` names the limit or the rule, never the offending value.
pub(crate) fn invalid(what: &'static str) -> StoreError {
    StoreError::Validation(what.to_owned())
}

/// The store was closed (its writer stopped): every later call fails, none waits.
pub(crate) fn closed() -> StoreError {
    database("store closed")
}

/// A job panicked on the writer thread (the thread lives on, the next call works): not "closed".
pub(crate) fn job_failed() -> StoreError {
    database("store job failed")
}

/// Maps a rusqlite failure of `op` to a `StoreError` and logs it once. Only the operation and the
/// SQLite (extended) result code are logged: the error text of SQLite can name a table, a column and
/// a conflicting constraint, and a conversion error can name a type; none of it is needed.
pub(crate) fn sql(op: &'static str) -> impl FnOnce(rusqlite::Error) -> StoreError {
    move |e| {
        let code = e.sqlite_error().map(|e| e.extended_code);
        tracing::error!(op, code, "whatsapp backend.db store failure");
        database(op)
    }
}

/// Why `StoreOwner::open` did not give a store. The file is not touched by a wrong key.
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
        /// The schema component (`whatsapp` or `core_outbox`).
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
    /// Any other failure of the database layer (the migration, the pragmas, the thread).
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
