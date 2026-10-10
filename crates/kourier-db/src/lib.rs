//! SQLCipher databases for the core and every backend (docs/architecture.md §5.3-§5.7).
//!
//! - [`open()`] / [`open_read_only`]: open a file with a raw 32-byte [`DbKey`] and the pragmas of
//!   §5.3 (WAL, `synchronous = FULL`, `secure_delete`, …). A wrong key fails with
//!   [`Error::WrongKey`] before anything is written.
//! - [`migrate`]: the forward-only migration runner with one `schema_version` row per component;
//!   a database newer than the binary is refused ([`Error::TooNew`]).
//! - [`ZoneDb`]: the synchronous zone API of §6.3 (`BEGIN IMMEDIATE … COMMIT`, no awaits inside).
//! - [`outbox`]: the generic `core_outbox` table and its [`Forwarder`] (§5.6), shared by every
//!   backend; rows are opaque versioned bytes here. Rows the core refuses for good are
//!   quarantined in `core_outbox_poison` ([`Refusal::Reject`]).
//! - [`Writer`]: a dedicated writer thread with a two-priority, per-lane round-robin job queue
//!   (the `core.db` writer of §2.5).
//! - [`failpoint`]: `KOURIER_FAILPOINT` kill points for crash-safety tests (§10.4).
//!
//! The engine is SQLCipher 4 with a RustCrypto provider (`bundled-sqlcipher-custom-crypto`), so
//! no OpenSSL is linked next to libsignal's BoringSSL. Nothing outside this crate names the
//! engine, which keeps a switch to SQLite3 Multiple Ciphers a dependency change.

pub mod failpoint;
mod key;
mod migrate;
mod open;
pub mod outbox;
mod writer;
mod zone;

pub use key::DbKey;
pub use migrate::{Migration, MigrationOutcome, Migrations, Step, migrate, schema_version};
pub use open::{checkpoint_truncate, open, open_read_only, try_checkpoint_truncate};
pub use outbox::{
    Batch, Forwarder, ForwarderConfig, OutboxRow, OutboxSource, PoisonRow, Refusal, Rejected,
};
pub use writer::{Priority, Writer, WriterHandle};
pub use zone::{Tx, ZoneDb};

/// Re-exported so callers name the same rusqlite as this crate (one SQLCipher build).
pub use rusqlite;

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The key does not decrypt the file, or the file is not a Kourier (SQLCipher) database.
    #[error("wrong key, or not a Kourier database")]
    WrongKey,
    /// The linked SQLite has no SQLCipher codec (a build misconfiguration).
    #[error("SQLCipher is not available in this build")]
    NoCipher,
    /// The file was written by a newer Kourier (§5.7): never opened, never downgraded.
    #[error(
        "the {component} schema is version {found}, newer than this build supports ({supported}); \
         update Kourier"
    )]
    TooNew {
        /// Schema component (`core`, `signal`, `core_outbox`, …).
        component: String,
        /// Version recorded in the file.
        found: i64,
        /// Latest version this binary knows.
        supported: i64,
    },
    /// A migration list is malformed (gaps, duplicates, file name not matching the number).
    #[error("invalid migration list for {component}: {reason}")]
    InvalidMigrations {
        /// Schema component.
        component: &'static str,
        /// What is wrong.
        reason: String,
    },
    /// One migration step failed; the whole run was rolled back.
    #[error("migration {component} {version} ({name}) failed: {source}")]
    Migration {
        /// Schema component.
        component: &'static str,
        /// Step number.
        version: u32,
        /// Step name (`NNNN_name`).
        name: &'static str,
        /// The underlying failure.
        source: Box<Error>,
    },
    /// A table rebuild left rows whose foreign keys point nowhere (`PRAGMA foreign_key_check`).
    #[error("{0} rows violate foreign keys after a table rebuild")]
    ForeignKeys(i64),
    /// `journal_mode = WAL` was not accepted (for example on a read-only medium).
    #[error("journal mode is {0}, expected wal")]
    JournalMode(String),
    /// A zone was opened inside another zone on the same connection (§6.3 rule 1).
    #[error("zones never nest")]
    NestedZone,
    /// A `core_outbox` row was appended outside a transaction (§4.1 principle 6).
    #[error("core_outbox rows are written only inside the transaction that implies them")]
    OutsideTransaction,
    /// The writer thread has stopped, or the job panicked.
    #[error("the database writer thread has stopped")]
    WriterGone,
    /// Any other SQLite error.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// File system errors (spawning the writer thread, file checks).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Milliseconds since the Unix epoch (for `created_at` columns).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
