//! Our prekeys: one-time EC prekeys, signed EC prekeys, and Kyber prekeys with last-resort
//! triples (§6.14 item 15; `LS/rust/protocol/src/storage/traits.rs:117-140`).

use async_trait::async_trait;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_protocol::{
    CiphertextMessageType, GenericSignedPreKey as _, KyberPreKeyId, KyberPreKeyRecord,
    KyberPreKeyStore, PreKeyId, PreKeyRecord, PreKeyStore, PublicKey, SignalProtocolError,
    SignedPreKeyId, SignedPreKeyRecord, SignedPreKeyStore,
};

use super::{IdentityKind, sql_error};

/// One-time EC prekeys of one own identity.
pub(crate) struct SqlPreKeyStore<'c> {
    conn: &'c Connection,
    kind: IdentityKind,
    now: i64,
}

impl<'c> SqlPreKeyStore<'c> {
    pub(super) fn new(conn: &'c Connection, kind: IdentityKind, now: i64) -> Self {
        Self { conn, kind, now }
    }
}

#[async_trait(?Send)]
impl PreKeyStore for SqlPreKeyStore<'_> {
    async fn get_pre_key(&self, id: PreKeyId) -> Result<PreKeyRecord, SignalProtocolError> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT record FROM prekeys WHERE identity = ?1 AND key_id = ?2")
            .and_then(|mut stmt| {
                stmt.query_row((self.kind.as_str(), u32::from(id)), |r| r.get(0))
                    .optional()
            })
            .map_err(|e| sql_error("load prekey", &e))?;
        PreKeyRecord::deserialize(&bytes.ok_or(SignalProtocolError::InvalidPreKeyId)?)
    }

    async fn save_pre_key(
        &mut self,
        _id: PreKeyId,
        record: &PreKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        insert_prekey(self.conn, self.kind, record, self.now)
            .map_err(|e| sql_error("save prekey", &e))
    }

    async fn remove_pre_key(&mut self, id: PreKeyId) -> Result<(), SignalProtocolError> {
        self.conn
            .prepare_cached("DELETE FROM prekeys WHERE identity = ?1 AND key_id = ?2")
            .and_then(|mut stmt| stmt.execute((self.kind.as_str(), u32::from(id))))
            .map(drop)
            .map_err(|e| sql_error("remove prekey", &e))
    }
}

/// Signed EC prekeys of one own identity.
pub(crate) struct SqlSignedPreKeyStore<'c> {
    conn: &'c Connection,
    kind: IdentityKind,
}

impl<'c> SqlSignedPreKeyStore<'c> {
    pub(super) fn new(conn: &'c Connection, kind: IdentityKind) -> Self {
        Self { conn, kind }
    }
}

#[async_trait(?Send)]
impl SignedPreKeyStore for SqlSignedPreKeyStore<'_> {
    async fn get_signed_pre_key(
        &self,
        id: SignedPreKeyId,
    ) -> Result<SignedPreKeyRecord, SignalProtocolError> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT record FROM signed_prekeys WHERE identity = ?1 AND key_id = ?2")
            .and_then(|mut stmt| {
                stmt.query_row((self.kind.as_str(), u32::from(id)), |r| r.get(0))
                    .optional()
            })
            .map_err(|e| sql_error("load signed prekey", &e))?;
        SignedPreKeyRecord::deserialize(&bytes.ok_or(SignalProtocolError::InvalidSignedPreKeyId)?)
    }

    async fn save_signed_pre_key(
        &mut self,
        _id: SignedPreKeyId,
        record: &SignedPreKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        insert_signed_prekey(self.conn, self.kind, record, false)
            .map_err(|e| sql_error("save signed prekey", &e))
    }
}

/// Kyber prekeys of one own identity: one-time keys are deleted on use, the last-resort key
/// stays and remembers every (signed prekey, base key) it was used with.
pub(crate) struct SqlKyberPreKeyStore<'c> {
    conn: &'c Connection,
    kind: IdentityKind,
    now: i64,
}

impl<'c> SqlKyberPreKeyStore<'c> {
    pub(super) fn new(conn: &'c Connection, kind: IdentityKind, now: i64) -> Self {
        Self { conn, kind, now }
    }
}

#[async_trait(?Send)]
impl KyberPreKeyStore for SqlKyberPreKeyStore<'_> {
    async fn get_kyber_pre_key(
        &self,
        id: KyberPreKeyId,
    ) -> Result<KyberPreKeyRecord, SignalProtocolError> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT record FROM kyber_prekeys WHERE identity = ?1 AND key_id = ?2")
            .and_then(|mut stmt| {
                stmt.query_row((self.kind.as_str(), u32::from(id)), |r| r.get(0))
                    .optional()
            })
            .map_err(|e| sql_error("load Kyber prekey", &e))?;
        KyberPreKeyRecord::deserialize(&bytes.ok_or(SignalProtocolError::InvalidKyberPreKeyId)?)
    }

    async fn save_kyber_pre_key(
        &mut self,
        _id: KyberPreKeyId,
        record: &KyberPreKeyRecord,
    ) -> Result<(), SignalProtocolError> {
        // libsignal itself never saves Kyber keys; our generator marks last-resort keys with
        // `insert_kyber_prekey` directly. A key saved through the trait is one-time.
        insert_kyber_prekey(self.conn, self.kind, record, false, false, self.now)
            .map_err(|e| sql_error("save Kyber prekey", &e))
    }

    async fn mark_kyber_pre_key_used(
        &mut self,
        kyber_id: KyberPreKeyId,
        signed_id: SignedPreKeyId,
        base_key: &PublicKey,
    ) -> Result<(), SignalProtocolError> {
        let last_resort: Option<bool> = self
            .conn
            .prepare_cached(
                "SELECT last_resort FROM kyber_prekeys WHERE identity = ?1 AND key_id = ?2",
            )
            .and_then(|mut stmt| {
                stmt.query_row((self.kind.as_str(), u32::from(kyber_id)), |r| r.get(0))
                    .optional()
            })
            .map_err(|e| sql_error("load Kyber prekey", &e))?;
        match last_resort {
            None => Err(SignalProtocolError::InvalidKyberPreKeyId),
            Some(false) => self
                .conn
                .prepare_cached("DELETE FROM kyber_prekeys WHERE identity = ?1 AND key_id = ?2")
                .and_then(|mut stmt| stmt.execute((self.kind.as_str(), u32::from(kyber_id))))
                .map(drop)
                .map_err(|e| sql_error("remove Kyber prekey", &e)),
            Some(true) => {
                let inserted = self
                    .conn
                    .prepare_cached(
                        "INSERT INTO kyber_triples(identity, kyber_id, signed_id, base_key,
                           used_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
                    )
                    .and_then(|mut stmt| {
                        stmt.execute((
                            self.kind.as_str(),
                            u32::from(kyber_id),
                            u32::from(signed_id),
                            &base_key.serialize()[..],
                            self.now,
                        ))
                    })
                    .map_err(|e| sql_error("record Kyber triple", &e))?;
                if inserted == 0 {
                    // The same last-resort key, signed prekey and base key again: a replayed
                    // PreKeySignalMessage (Desktop's markKyberTripleSeenOrFail).
                    return Err(SignalProtocolError::InvalidMessage(
                        CiphertextMessageType::PreKey,
                        String::from("reused base key"),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Stores a one-time EC prekey.
pub(crate) fn insert_prekey(
    conn: &Connection,
    kind: IdentityKind,
    record: &PreKeyRecord,
    now: i64,
) -> kourier_db::rusqlite::Result<()> {
    let id = record.id().map(u32::from).map_err(to_sql)?;
    let bytes = record.serialize().map_err(to_sql)?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO prekeys(identity, key_id, record, created_at) VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute((kind.as_str(), id, bytes, now))?;
    Ok(())
}

/// Stores a signed EC prekey; `confirmed` when the server already has it (the link request
/// carried it, or its upload was accepted, §6.12).
pub(crate) fn insert_signed_prekey(
    conn: &Connection,
    kind: IdentityKind,
    record: &SignedPreKeyRecord,
    confirmed: bool,
) -> kourier_db::rusqlite::Result<()> {
    let id = record.id().map(u32::from).map_err(to_sql)?;
    let created_at = record
        .timestamp()
        .map(|t| i64::try_from(t.epoch_millis()).unwrap_or(i64::MAX))
        .map_err(to_sql)?;
    let bytes = record.serialize().map_err(to_sql)?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO signed_prekeys(identity, key_id, record, created_at, confirmed)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute((kind.as_str(), id, bytes, created_at, confirmed))?;
    Ok(())
}

/// Stores a Kyber prekey; `last_resort` keys are kept after use and guarded by triples, and
/// are `confirmed` once the server has them (as for signed prekeys).
pub(crate) fn insert_kyber_prekey(
    conn: &Connection,
    kind: IdentityKind,
    record: &KyberPreKeyRecord,
    last_resort: bool,
    confirmed: bool,
    now: i64,
) -> kourier_db::rusqlite::Result<()> {
    let id = record.id().map(u32::from).map_err(to_sql)?;
    let bytes = record.serialize().map_err(to_sql)?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO kyber_prekeys(identity, key_id, record, last_resort, created_at,
           confirmed) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute((kind.as_str(), id, bytes, last_resort, now, confirmed))?;
    Ok(())
}

/// A libsignal serialization failure while writing a row (cannot happen for records we built).
fn to_sql(e: SignalProtocolError) -> kourier_db::rusqlite::Error {
    kourier_db::rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
        crate::error::protocol_error_kind(&e),
    )))
}
