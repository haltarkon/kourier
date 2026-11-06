//! TH6 L2b: THE TRANSACTIONS of the fake mediator (`TP/md.md` "Transactions", `TP/md-d2m.proto` 0x40-0x45):
//! what the mediator records of them and the test side's view of their scopes. The lock itself lives in
//! the mediator's state (`d2m.rs`, `begin_transaction` / `commit_transaction` / the holder's end / the TTL in
//! `tick`); this file holds the types and the scope's seal and open on the fake's own crypto (DGTSK, which
//! the mediator never holds: the test side opens what the mediator only carries). Rule 9 holds: the
//! generated protobuf types and the fake's own crypto only.
//!
//! THE FAKE'S RULES (each [U: real server] where the spec is silent):
//! - ONE transaction at a time per DEVICE GROUP: a `BeginTransaction` while another connection holds the
//!   lock gets `TransactionRejected { device_id, encrypted_scope }` of the holder (its scope bytes as the
//!   holder sent them); else `BeginTransactionAck`. A second `BeginTransaction` of the holder is the
//!   violation `d2m-begin-while-holding` (4010).
//! - A `Reflect` of the holder INSIDE its transaction is acked at once (`ReflectAck` with the time it was
//!   taken; `Knobs::withhold_reflect_acks` holds it as any other) and reaches the other devices only at the
//!   commit, in order ("Reflection messages from the device to the mediator server will only be reflected
//!   once the transaction is committed"). [U: real server] whether the ack comes before the commit (Desktop
//!   awaits it inside the transaction, `manager.ts:386-392`). An ephemeral reflect is not held (it is never
//!   queued) [U].
//! - `CommitTransaction` of the holder: the held reflects go out, `CommitTransactionAck` to the holder,
//!   `TransactionEnded { device_id, encrypted_scope }` to every OTHER logged-in connection of the group.
//!   A commit with nothing held is the graceful end the spec asks for when the precondition fails after
//!   `BeginTransactionAck` (`TP/md.md:96-98`, critic T-L2). A commit of a connection that holds no
//!   transaction is the violation `d2m-commit-without-transaction` (4010).
//! - THE HOLDER'S END (its connection closed by either side, lost, or 4011): the transaction ends, its
//!   held reflects are DROPPED (traced `transaction-reflects-dropped`), and every other logged-in connection
//!   of the group gets `TransactionEnded`. [U: real server] that the uncommitted reflects are dropped.
//! - THE TTL: `BeginTransaction.ttl` seconds, `0` = the server's maximum (`Knobs::max_transaction_ttl`, the
//!   fake's 60 s [U: real server]); a larger TTL is cut to it. At the TTL the mediator closes the holder with
//!   `4011` ("Transaction TTL reached", `TP/md-d2m.struct.yml`) and the transaction ends as above.
//! - The scope: the mediator never opens it. [`MockWorld::transaction_scopes`] opens every scope a
//!   `BeginTransaction` carried with DGTSK and records `d2m-transaction-scope-unopened` for one that does not
//!   open or decode.

use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::transaction_scope::Scope;
use prost::Message as _;

use super::MockWorld;
use super::crypto::{DeviceGroupKeys, open_nonce_ahead, seal_nonce_ahead};
use super::d2m::ConnId;

/// How a transaction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnEnd {
    /// The holder committed it.
    Committed,
    /// The holder's connection ended (by either side, or lost).
    Disconnected,
    /// The TTL ran out: the mediator closed the holder with 4011.
    Ttl,
}

/// One moment of a transaction, in the order the mediator handled it. `Debug` prints numbers only.
#[derive(Clone, PartialEq, Eq)]
pub enum TxnSeen {
    /// `BeginTransaction` of `conn` took the lock; `ttl_ms` is the TTL the fake applies.
    Begun {
        conn: ConnId,
        device: u64,
        ttl_ms: u64,
        /// The encrypted scope exactly as the client sent it.
        scope: Vec<u8>,
    },
    /// `BeginTransaction` of `conn` was rejected: `holder` holds the lock.
    Rejected {
        conn: ConnId,
        device: u64,
        holder: u64,
        /// The encrypted scope the REJECTED begin carried.
        scope: Vec<u8>,
    },
    /// A reflect of the holder was held inside its transaction.
    Held { conn: ConnId, reflect_id: u32 },
    /// The transaction of `device` ended; `released` reflects went out (0 unless committed), `dropped`
    /// were dropped.
    Ended {
        conn: ConnId,
        device: u64,
        how: TxnEnd,
        released: usize,
        dropped: usize,
    },
}

impl std::fmt::Debug for TxnSeen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Begun {
                conn,
                device,
                ttl_ms,
                scope,
            } => write!(
                f,
                "Begun(conn {}, device {device:#x}, ttl {ttl_ms} ms, scope {} B)",
                conn.0,
                scope.len()
            ),
            Self::Rejected {
                conn,
                device,
                holder,
                ..
            } => write!(
                f,
                "Rejected(conn {}, device {device:#x}, holder {holder:#x})",
                conn.0
            ),
            Self::Held { conn, reflect_id } => {
                write!(f, "Held(conn {}, reflect {reflect_id})", conn.0)
            }
            Self::Ended {
                conn,
                device,
                how,
                released,
                dropped,
            } => write!(
                f,
                "Ended(conn {}, device {device:#x}, {how:?}, released {released}, dropped {dropped})",
                conn.0
            ),
        }
    }
}

/// The lock of one device group while a transaction runs (part of the mediator's group state).
#[derive(Clone)]
pub struct Txn {
    /// The connection that holds it and its device.
    pub holder: ConnId,
    pub device: u64,
    /// The encrypted scope as the holder sent it (what `TransactionRejected` / `TransactionEnded` carry).
    pub scope: Vec<u8>,
    /// When the mediator ends it (virtual ms).
    pub deadline_ms: u64,
    /// The holder's reflects taken inside it, in order: (reflect-id, flags, sealed envelope, taken at ms).
    pub held: Vec<(u32, u16, Vec<u8>, u64)>,
}

/// A `d2d.TransactionScope` of `scope`, sealed nonce-ahead with DGTSK under `nonce` (what a client puts in
/// `BeginTransaction.encrypted_scope`).
pub fn seal_scope(keys: &DeviceGroupKeys, nonce: &[u8; 24], scope: Scope) -> Vec<u8> {
    let plain = d2d::TransactionScope {
        scope: scope as i32,
    }
    .encode_to_vec();
    seal_nonce_ahead(&keys.transaction_scope, nonce, &plain)
}

/// Opens an encrypted scope with DGTSK: `None` when it does not open, does not decode, or names a scope
/// the pinned spec does not know.
pub fn open_scope(keys: &DeviceGroupKeys, sealed: &[u8]) -> Option<Scope> {
    let plain = open_nonce_ahead(&keys.transaction_scope, sealed)?;
    let scope = d2d::TransactionScope::decode(plain.as_slice()).ok()?;
    Scope::try_from(scope.scope).ok()
}

impl MockWorld {
    /// The scope of every `BeginTransaction` the mediator took or rejected, in order, opened with the group's
    /// DGTSK (`keys`: the test side's, the mediator never holds them); one that does not open is `None` and
    /// recorded as `d2m-transaction-scope-unopened` (with its connection). Calling it twice records twice.
    pub fn transaction_scopes(&self, keys: &DeviceGroupKeys) -> Vec<Option<Scope>> {
        let mut out = Vec::new();
        for seen in self.mediator().transactions() {
            let (conn, scope) = match &seen {
                TxnSeen::Begun { conn, scope, .. } | TxnSeen::Rejected { conn, scope, .. } => {
                    (*conn, scope)
                }
                TxnSeen::Held { .. } | TxnSeen::Ended { .. } => continue,
            };
            let opened = open_scope(keys, scope);
            if opened.is_none() {
                self.violations()
                    .record("d2m-transaction-scope-unopened", &[("conn", conn.0)]);
            }
            out.push(opened);
        }
        out
    }
}
