//! Errors of the Threema store. A failure carries the NAME of the operation and, for a failure of the
//! database layer, SQLite's EXTENDED result code, and nothing else: no Threema ID, key, cookie, nonce,
//! label or SQL text (architecture §9.1). The SQLite error itself is logged once, at the place that
//! maps it, with the operation and the result code only.
//!
//! THE KIND (TH5 round A, H-1 / S1): the reflection loop decides by the error whether a message may be
//! dropped. [`StoreError::is_content`] is `true` for a failure that is the CONTENT's (a constraint, a
//! type mismatch, a value too big, a check this crate made itself, a row that cannot be decoded): the
//! same envelope would fail again on a healthy file. Everything else with a SQLite code, and a
//! database-layer failure with none, is the STORE's (a full disk, an I/O error, no memory, a busy or
//! locked file, a read-only or corrupt one, a code this build does not know): a message must not be
//! acked or dropped then.

use kourier_db::rusqlite::{self, ErrorCode};

/// Why a message to send was refused before anything of it was written (TH6a, `send::prepare`): a fixed reason, never
/// an input. Each is deterministic: the same message is refused again, so the caller's zone rolls back and a later
/// step decides what the user is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendRefused {
    /// A `local` that is not `msg_id`'s text form of a message id.
    #[error("the id of a message to send is not readable")]
    BadLocal,
    /// A contact the file does not hold.
    #[error("the contact of a message to send is not stored")]
    UnknownContact,
    /// A group the file does not hold.
    #[error("the group of a message to send is not stored")]
    UnknownGroup,
    /// A group the stored state says the user left or was kicked from.
    #[error("the user is not a member of the group of a message to send")]
    NotAMember,
    /// A type no message has: `_INVALID_TYPE`, or a number the pinned enum lacks.
    #[error("the type of a message to send is none of the pinned ones")]
    UnknownType,
    /// A receiver or a group creator that is no Threema ID.
    #[error("a receiver of a message to send is no Threema ID")]
    BadId,
    /// One box over 8,170 bytes, or the sealed envelope over 65,516.
    #[error("a message to send is too large")]
    TooLarge,
    /// A type the chat server never acks (flag `0x04`, e.g. a typing indicator): volatile, it has no place in a
    /// persistent queue; sent as a frame with no row (TH7).
    #[error("a message to send is of a type that has no place in the send queue")]
    NotQueued,
}

/// A failure of the store: what failed, as a short fixed sentence, never an input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// A database failure of the named operation ("save identity failed").
    #[error("{0}")]
    Failed(&'static str),
    /// A failure of the database layer of the named operation. `code` is SQLite's EXTENDED result code
    /// (`SQLITE_CONSTRAINT_CHECK` = 275, `SQLITE_FULL` = 13, ...), or `None` when the layer itself failed
    /// without one (an I/O error of the writer, a nested zone). [`StoreError::is_content`] reads it.
    #[error("{op}")]
    Sql {
        /// The operation that failed.
        op: &'static str,
        /// SQLite's extended result code, if the failure came from SQLite.
        code: Option<i32>,
    },
    /// A stored row that cannot be read back as what it should be; names the record kind.
    #[error("{0}")]
    Unreadable(&'static str),
    /// The send queue's budget of this row's origin is spent (TH6a, `send::queue`): the row was not taken and
    /// the caller's zone rolls back. A `transmit` row first evicts the old rows nothing owes any more, if that makes
    /// room (S-M1); a `job` never evicts, and no row that is owed (attached, `acked`, `done`) is ever taken.
    #[error("the send queue is full")]
    QueueFull,
    /// A message to send was refused before anything of it was written (TH6a, [`SendRefused`]); a zone it fails rolls
    /// back whole.
    #[error("{0}")]
    Refused(SendRefused),
}

impl StoreError {
    /// Whether the failure is the CONTENT's and not the store's (module doc): a constraint, a type
    /// mismatch or a value too big (SQLite's primary codes, whatever the extended one), a failure this
    /// crate made itself ([`StoreError::Failed`]) or a row that cannot be read back
    /// ([`StoreError::Unreadable`]), or a message to send that was refused ([`StoreError::Refused`]: the same
    /// message is refused again). A code this build does not know, and a database-layer failure
    /// without a code, are the store's: the safe answer is "nothing is acked". A full send queue is not the
    /// content's either: the same row fits once the queue has drained, so a zone it rolls back acks nothing.
    pub(crate) fn is_content(&self) -> bool {
        match self {
            Self::Failed(_) | Self::Unreadable(_) | Self::Refused(_) => true,
            Self::QueueFull | Self::Sql { code: None, .. } => false,
            Self::Sql {
                code: Some(code), ..
            } => matches!(
                rusqlite::ffi::Error::new(*code).code,
                ErrorCode::ConstraintViolation | ErrorCode::TypeMismatch | ErrorCode::TooBig
            ),
        }
    }
}

/// `Result` with [`StoreError`].
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Maps a rusqlite failure of `op` to a [`StoreError`] and logs it once. Only the operation and the
/// SQLite (extended) result code are logged: the error text of SQLite can name a table, a column
/// and a constraint, and a conversion error can name a type; none of it is needed.
pub(crate) fn sql(op: &'static str) -> impl FnOnce(rusqlite::Error) -> StoreError {
    move |e| {
        let code = e.sqlite_error().map(|e| e.extended_code);
        tracing::error!(op, code, "threema backend.db store failure");
        match code {
            Some(code) => StoreError::Sql {
                op,
                code: Some(code),
            },
            // No SQLite code: a conversion of a value (a row that is not what it should be): the
            // content's own failure.
            None => StoreError::Failed(op),
        }
    }
}

/// What a zone itself can fail with (`BEGIN`, `COMMIT`, a nested zone): the database layer's kind
/// is logged, the error is a fixed sentence.
impl From<kourier_db::Error> for StoreError {
    fn from(e: kourier_db::Error) -> Self {
        let code = match &e {
            kourier_db::Error::Sqlite(e) => e.sqlite_error().map(|e| e.extended_code),
            _ => None,
        };
        let kind = match e {
            kourier_db::Error::NestedZone => "nested zone",
            kourier_db::Error::Sqlite(_) => "sqlite",
            _ => "database",
        };
        tracing::error!(kind, code, "threema backend.db transaction failure");
        StoreError::Sql {
            op: "transaction failed",
            code,
        }
    }
}

/// Why the account's `backend.db` did not open. A wrong key, a newer schema and a foreign file
/// leave the file untouched.
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
        /// The schema component (`threema` or `core_outbox`).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sql_code(code: i32) -> StoreError {
        StoreError::Sql {
            op: "a test operation failed",
            code: Some(code),
        }
    }

    /// H-1 / S1: the content's codes (a constraint with any of its extended codes, a type mismatch, a value
    /// too big) and this crate's own failures are content; every other SQLite code, one this build does not
    /// know and a layer failure without a code are the store's. The literals are SQLite's documented codes
    /// (`sqlite.org/rescode.html`), not read from the crate's table.
    #[test]
    fn the_kind_of_a_failure_is_read_from_its_code() {
        let content = [
            19,   // SQLITE_CONSTRAINT
            275,  // SQLITE_CONSTRAINT_CHECK
            787,  // SQLITE_CONSTRAINT_FOREIGNKEY
            1555, // SQLITE_CONSTRAINT_PRIMARYKEY
            2067, // SQLITE_CONSTRAINT_UNIQUE
            1299, // SQLITE_CONSTRAINT_NOTNULL
            20,   // SQLITE_MISMATCH
            18,   // SQLITE_TOOBIG
        ];
        for code in content {
            assert!(sql_code(code).is_content(), "code {code} is the content's");
        }
        let store = [
            13,   // SQLITE_FULL
            10,   // SQLITE_IOERR
            778,  // SQLITE_IOERR_WRITE
            266,  // SQLITE_IOERR_READ
            3082, // SQLITE_IOERR_NOMEM
            7,    // SQLITE_NOMEM
            5,    // SQLITE_BUSY
            261,  // SQLITE_BUSY_RECOVERY
            6,    // SQLITE_LOCKED
            8,    // SQLITE_READONLY
            11,   // SQLITE_CORRUPT
            14,   // SQLITE_CANTOPEN
            26,   // SQLITE_NOTADB
            3,    // SQLITE_PERM
            9,    // SQLITE_INTERRUPT
            21,   // SQLITE_MISUSE
            // No code of SQLite's list, but its low byte (9999 & 0xff = 15) is SQLITE_PROTOCOL's
            // primary code: the store's either way (TH5a re-check N-4).
            9999,
        ];
        for code in store {
            assert!(!sql_code(code).is_content(), "code {code} is the store's");
        }
        assert!(
            !StoreError::Sql {
                op: "x",
                code: None
            }
            .is_content(),
            "a layer failure without a code is the store's"
        );
        assert!(StoreError::Failed("x").is_content());
        assert!(StoreError::Unreadable("x").is_content());
    }

    /// `sql()` keeps the EXTENDED code of a real constraint failure, and a conversion failure (no SQLite
    /// code) stays the crate's own `Failed`; the error prints the operation and nothing else.
    #[test]
    fn sql_carries_the_extended_code_of_the_failure() {
        let conn = rusqlite::Connection::open_in_memory().expect("an in-memory file");
        conn.execute_batch("CREATE TABLE t (n INTEGER NOT NULL CHECK (n > 0)) STRICT")
            .expect("a table");
        let check = conn
            .execute("INSERT INTO t (n) VALUES (0)", [])
            .expect_err("the CHECK refuses");
        let mapped = sql("insert a test row failed")(check);
        assert_eq!(
            mapped,
            StoreError::Sql {
                op: "insert a test row failed",
                code: Some(275)
            },
            "the extended code of a CHECK failure"
        );
        assert!(mapped.is_content());
        assert_eq!(mapped.to_string(), "insert a test row failed");
        let conversion = conn
            .query_row("SELECT 'text'", [], |r| r.get::<_, i64>(0))
            .expect_err("a text is no integer");
        assert_eq!(
            sql("read a test row failed")(conversion),
            StoreError::Failed("read a test row failed"),
            "a failure with no SQLite code"
        );
    }

    /// The zone's own failure keeps its code too, and a layer failure without one is the store's.
    #[test]
    fn a_zone_failure_keeps_its_code() {
        let full = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(13), None);
        let mapped = StoreError::from(kourier_db::Error::Sqlite(full));
        assert_eq!(
            mapped,
            StoreError::Sql {
                op: "transaction failed",
                code: Some(13)
            }
        );
        assert!(!mapped.is_content(), "a full disk is the store's");
        let nested = StoreError::from(kourier_db::Error::NestedZone);
        assert_eq!(
            nested,
            StoreError::Sql {
                op: "transaction failed",
                code: None
            }
        );
        assert!(!nested.is_content());
    }
}
