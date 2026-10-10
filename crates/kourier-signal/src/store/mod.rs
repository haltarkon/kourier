//! libsignal's protocol stores over `backend.db` (§6.3; ports `SD/ts/LibSignalStores.node.ts` and
//! the store half of `SD/ts/SignalProtocolStore.preload.ts`).
//!
//! The stores borrow the connection of an open zone ([`kourier_db::Tx`] derefs to it), so every
//! read and write they do belongs to that zone's transaction: a zone that rolls back takes the
//! ratchet steps, identity saves, prekey deletions and Kyber triples with it. Their futures are
//! always ready (synchronous rusqlite underneath), so zone bodies drive libsignal's async API
//! with [`ready`] and never await. There is no in-memory cache yet, so there is no overlay to
//! apply on commit either. One set of stores exists per own identity: ACI and PNI.

mod identity;
mod prekeys;
mod sender_key;
mod session;

use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use kourier_db::rusqlite::Connection;
use libsignal_protocol::{IdentityKeyPair, ProtocolAddress, SignalProtocolError};

pub(crate) use identity::{
    IdentityChangeRecord, SqlIdentityStore, load_identity_key, save_own_identity,
};
pub(crate) use prekeys::{
    SqlKyberPreKeyStore, SqlPreKeyStore, SqlSignedPreKeyStore, insert_kyber_prekey, insert_prekey,
    insert_signed_prekey,
};
pub(crate) use sender_key::SqlSenderKeyStore;
pub(crate) use session::{SqlSessionStore, archive_all_sessions, archive_session, session_devices};

use crate::error::{Error, Result};

/// Which of our two identities a store (or a key, or a session) belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IdentityKind {
    /// The account identity (ACI).
    Aci,
    /// The phone-number identity (PNI).
    Pni,
}

impl IdentityKind {
    /// Both kinds, ACI first.
    pub(crate) const ALL: [Self; 2] = [Self::Aci, Self::Pni];

    /// The SQL value.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Aci => "aci",
            Self::Pni => "pni",
        }
    }
}

impl fmt::Display for IdentityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The five protocol stores of one own identity, over one zone's connection.
pub(crate) struct Stores<'c> {
    /// Identity keys (ours and theirs).
    pub(crate) identity: SqlIdentityStore<'c>,
    /// Sessions.
    pub(crate) session: SqlSessionStore<'c>,
    /// One-time EC prekeys.
    pub(crate) prekey: SqlPreKeyStore<'c>,
    /// Signed EC prekeys.
    pub(crate) signed: SqlSignedPreKeyStore<'c>,
    /// Kyber prekeys and last-resort triples.
    pub(crate) kyber: SqlKyberPreKeyStore<'c>,
}

impl<'c> Stores<'c> {
    /// The stores of identity `kind` (`identity`, `registration_id`) on `conn`; `now` (ms)
    /// stamps the rows they write.
    pub(crate) fn new(
        conn: &'c Connection,
        kind: IdentityKind,
        identity: IdentityKeyPair,
        registration_id: u32,
        now: i64,
    ) -> Self {
        Self {
            identity: SqlIdentityStore::new(conn, identity, registration_id, now),
            session: SqlSessionStore::new(conn, kind, now),
            prekey: SqlPreKeyStore::new(conn, kind, now),
            signed: SqlSignedPreKeyStore::new(conn, kind),
            kyber: SqlKyberPreKeyStore::new(conn, kind, now),
        }
    }
}

/// Polls a store-backed libsignal future once. Our stores never pend (synchronous SQL), so a
/// pending future means a store that awaits something: a bug, reported instead of blocking the
/// zone (§6.3).
pub(crate) fn ready<F: Future>(future: F) -> Result<F::Output> {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(out) => Ok(out),
        Poll::Pending => Err(Error::Internal("store future pended inside a zone")),
    }
}

/// The message of a store failure inside a libsignal error (see [`is_store_failure`]).
const STORE_FAILURE: &str = "backend.db failure";

/// A store failure as libsignal's error. Only the operation name travels (log-safe); the SQL
/// error goes to the log once, here.
fn sql_error(op: &'static str, e: &kourier_db::rusqlite::Error) -> SignalProtocolError {
    tracing::error!(op, error = %e, "backend.db store failure");
    SignalProtocolError::InvalidState(op, String::from(STORE_FAILURE))
}

/// Whether a libsignal error is one of our stores failing (a database error, never a protocol
/// outcome): the zone must abort instead of treating the envelope as undecryptable.
pub(crate) fn is_store_failure(e: &SignalProtocolError) -> bool {
    matches!(e, SignalProtocolError::InvalidState(_, detail) if detail == STORE_FAILURE)
}

/// The device id of an address as stored.
fn device_of(address: &ProtocolAddress) -> i64 {
    i64::from(u32::from(address.device_id()))
}

#[cfg(test)]
pub(crate) mod tests;
