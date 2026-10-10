//! `IdentityKeyStore` over the `identities` table (`SD/ts/SignalProtocolStore.preload.ts`
//! `saveIdentity`, `isTrustedIdentity`).

use async_trait::async_trait;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_protocol::{
    Direction, IdentityChange, IdentityKey, IdentityKeyPair, IdentityKeyStore, ProtocolAddress,
    SignalProtocolError,
};

use super::{device_of, sql_error};

/// `identities.verified`: the user has not compared safety numbers.
const DEFAULT: i64 = 0;
/// `identities.verified`: verified (our own identities, or the user compared safety numbers).
const VERIFIED: i64 = 1;
/// `identities.verified`: was verified, then the key changed.
const UNVERIFIED: i64 = 2;

/// A remote identity key that replaced a different one (stage 1 turns it into core events).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct IdentityChangeRecord {
    /// Their service id string.
    pub(crate) service_id: String,
    /// The new key.
    pub(crate) identity: IdentityKey,
}

/// Identity keys of one own identity's view. Remote identities are shared by both own
/// identities (they are keyed by the remote service id only, as in Desktop).
pub(crate) struct SqlIdentityStore<'c> {
    conn: &'c Connection,
    identity: IdentityKeyPair,
    registration_id: u32,
    now: i64,
    changes: Vec<IdentityChangeRecord>,
}

impl<'c> SqlIdentityStore<'c> {
    pub(super) fn new(
        conn: &'c Connection,
        identity: IdentityKeyPair,
        registration_id: u32,
        now: i64,
    ) -> Self {
        Self {
            conn,
            identity,
            registration_id,
            now,
            changes: Vec::new(),
        }
    }

    /// The identity changes `save_identity` recorded so far, oldest first.
    pub(crate) fn take_changes(&mut self) -> Vec<IdentityChangeRecord> {
        std::mem::take(&mut self.changes)
    }

    fn stored(&self, service_id: &str) -> Result<Option<(Vec<u8>, i64)>, SignalProtocolError> {
        self.conn
            .prepare_cached("SELECT identity_key, verified FROM identities WHERE service_id = ?1")
            .and_then(|mut stmt| {
                stmt.query_row([service_id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()
            })
            .map_err(|e| sql_error("load identity", &e))
    }

    /// Desktop's `archiveSiblingSessions`: after an identity change, every other device's
    /// session with this service id was built on the old key.
    fn archive_siblings(&self, address: &ProtocolAddress) -> Result<(), SignalProtocolError> {
        let siblings: Vec<(String, i64)> = self
            .conn
            .prepare_cached(
                "SELECT our_kind, device_id FROM sessions
                 WHERE their_service_id = ?1 AND device_id != ?2",
            )
            .and_then(|mut stmt| {
                stmt.query_map((address.name(), device_of(address)), |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect()
            })
            .map_err(|e| sql_error("list sibling sessions", &e))?;
        for (our_kind, device) in siblings {
            super::session::archive_row(self.conn, &our_kind, address.name(), device, self.now)?;
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl IdentityKeyStore for SqlIdentityStore<'_> {
    async fn get_identity_key_pair(&self) -> Result<IdentityKeyPair, SignalProtocolError> {
        Ok(self.identity)
    }

    async fn get_local_registration_id(&self) -> Result<u32, SignalProtocolError> {
        Ok(self.registration_id)
    }

    async fn save_identity(
        &mut self,
        address: &ProtocolAddress,
        identity: &IdentityKey,
    ) -> Result<IdentityChange, SignalProtocolError> {
        let key = identity.serialize();
        match self.stored(address.name())? {
            None => {
                self.conn
                    .prepare_cached(
                        "INSERT INTO identities(service_id, identity_key, verified, first_use,
                           updated_at) VALUES (?1, ?2, ?3, 1, ?4)",
                    )
                    .and_then(|mut stmt| {
                        stmt.execute((address.name(), &key[..], DEFAULT, self.now))
                    })
                    .map_err(|e| sql_error("save identity", &e))?;
                Ok(IdentityChange::NewOrUnchanged)
            }
            Some((stored, _)) if stored.as_slice() == &key[..] => {
                Ok(IdentityChange::NewOrUnchanged)
            }
            Some((_, verified)) => {
                // A verified contact whose key changed becomes "unverified" (Desktop's
                // VerifiedStatus.UNVERIFIED); the next send pauses until the user confirms.
                let verified = if verified == VERIFIED {
                    UNVERIFIED
                } else {
                    verified
                };
                self.conn
                    .prepare_cached(
                        "UPDATE identities SET identity_key = ?2, verified = ?3, first_use = 0,
                           updated_at = ?4 WHERE service_id = ?1",
                    )
                    .and_then(|mut stmt| {
                        stmt.execute((address.name(), &key[..], verified, self.now))
                    })
                    .map_err(|e| sql_error("replace identity", &e))?;
                self.archive_siblings(address)?;
                self.changes.push(IdentityChangeRecord {
                    service_id: address.name().to_owned(),
                    identity: *identity,
                });
                Ok(IdentityChange::ReplacedExisting)
            }
        }
    }

    async fn is_trusted_identity(
        &self,
        address: &ProtocolAddress,
        identity: &IdentityKey,
        direction: Direction,
    ) -> Result<bool, SignalProtocolError> {
        match direction {
            // Desktop trusts every identity for receiving; changes are recorded by save_identity.
            Direction::Receiving => Ok(true),
            // First use is trusted (TOFU); a different key, or a verified contact whose key
            // changed since (UNVERIFIED), needs the user's confirmation first (Desktop's
            // `isTrustedForSending`).
            Direction::Sending => Ok(match self.stored(address.name())? {
                None => true,
                Some((stored, verified)) => {
                    stored.as_slice() == &identity.serialize()[..] && verified != UNVERIFIED
                }
            }),
        }
    }

    async fn get_identity(
        &self,
        address: &ProtocolAddress,
    ) -> Result<Option<IdentityKey>, SignalProtocolError> {
        self.stored(address.name())?
            .map(|(key, _)| IdentityKey::decode(&key))
            .transpose()
    }
}

/// Records one of our own identity keys as verified (§6.5 step 6: "own marked verified").
pub(crate) fn save_own_identity(
    conn: &Connection,
    service_id: &str,
    identity: &IdentityKey,
    now: i64,
) -> kourier_db::rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4)
         ON CONFLICT(service_id) DO UPDATE SET identity_key = excluded.identity_key,
           verified = excluded.verified, updated_at = excluded.updated_at",
    )?
    .execute((service_id, &identity.serialize()[..], VERIFIED, now))?;
    Ok(())
}

/// The stored identity key of a remote service id string (`<uuid>` or `PNI:<uuid>`), if any.
pub(crate) fn load_identity_key(
    conn: &Connection,
    service_id: &str,
) -> crate::error::Result<Option<IdentityKey>> {
    let key: Option<Vec<u8>> = conn
        .prepare_cached("SELECT identity_key FROM identities WHERE service_id = ?1")?
        .query_row([service_id], |r| r.get(0))
        .optional()?;
    key.map(|key| {
        IdentityKey::decode(&key).map_err(|_| crate::error::Error::Decode("identity key"))
    })
    .transpose()
}
