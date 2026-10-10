//! `SessionStore` over the `sessions` table: `(our identity, their service id, device) →
//! SessionRecord`.

use async_trait::async_trait;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_protocol::{
    DeviceId, ProtocolAddress, SessionRecord, SessionStore, SignalProtocolError,
};

use super::{IdentityKind, device_of, sql_error};

/// Sessions of one own identity.
pub(crate) struct SqlSessionStore<'c> {
    conn: &'c Connection,
    kind: IdentityKind,
    now: i64,
}

impl<'c> SqlSessionStore<'c> {
    pub(crate) fn new(conn: &'c Connection, kind: IdentityKind, now: i64) -> Self {
        Self { conn, kind, now }
    }
}

#[async_trait(?Send)]
impl SessionStore for SqlSessionStore<'_> {
    async fn load_session(
        &self,
        address: &ProtocolAddress,
    ) -> Result<Option<SessionRecord>, SignalProtocolError> {
        load(
            self.conn,
            self.kind.as_str(),
            address.name(),
            device_of(address),
        )?
        .map(|bytes| SessionRecord::deserialize(&bytes))
        .transpose()
    }

    async fn store_session(
        &mut self,
        address: &ProtocolAddress,
        record: &SessionRecord,
    ) -> Result<(), SignalProtocolError> {
        store(
            self.conn,
            self.kind.as_str(),
            address.name(),
            device_of(address),
            &record.serialize()?,
            self.now,
        )
    }
}

fn load(
    conn: &Connection,
    our_kind: &str,
    their: &str,
    device: i64,
) -> Result<Option<Vec<u8>>, SignalProtocolError> {
    conn.prepare_cached(
        "SELECT record FROM sessions
         WHERE our_kind = ?1 AND their_service_id = ?2 AND device_id = ?3",
    )
    .and_then(|mut stmt| {
        stmt.query_row((our_kind, their, device), |r| r.get(0))
            .optional()
    })
    .map_err(|e| sql_error("load session", &e))
}

fn store(
    conn: &Connection,
    our_kind: &str,
    their: &str,
    device: i64,
    record: &[u8],
    now: i64,
) -> Result<(), SignalProtocolError> {
    conn.prepare_cached(
        "INSERT INTO sessions(our_kind, their_service_id, device_id, record, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(our_kind, their_service_id, device_id)
         DO UPDATE SET record = excluded.record, updated_at = excluded.updated_at",
    )
    .and_then(|mut stmt| stmt.execute((our_kind, their, device, record, now)))
    .map(drop)
    .map_err(|e| sql_error("store session", &e))
}

/// Archives the current state of one session row (keeps it as a previous state, so late
/// messages still decrypt, but never encrypts with it again).
pub(super) fn archive_row(
    conn: &Connection,
    our_kind: &str,
    their: &str,
    device: i64,
    now: i64,
) -> Result<(), SignalProtocolError> {
    let Some(bytes) = load(conn, our_kind, their, device)? else {
        return Ok(());
    };
    let mut record = SessionRecord::deserialize(&bytes)?;
    record.archive_current_state()?;
    store(conn, our_kind, their, device, &record.serialize()?, now)
}

/// Archives our session with `their` device `device` (409 extra devices, 410 stale devices).
pub(crate) fn archive_session(
    conn: &Connection,
    kind: IdentityKind,
    their: &str,
    device: DeviceId,
    now: i64,
) -> Result<(), SignalProtocolError> {
    archive_row(
        conn,
        kind.as_str(),
        their,
        i64::from(u32::from(device)),
        now,
    )
}

/// Archives every session with `their` (any device, either own identity): Desktop's
/// `archiveAllSessions` for an `END_SESSION` message.
pub(crate) fn archive_all_sessions(
    conn: &Connection,
    their: &str,
    now: i64,
) -> Result<(), SignalProtocolError> {
    let rows: Vec<(String, i64)> = conn
        .prepare_cached("SELECT our_kind, device_id FROM sessions WHERE their_service_id = ?1")
        .and_then(|mut stmt| {
            stmt.query_map([their], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        })
        .map_err(|e| sql_error("list sessions", &e))?;
    for (our_kind, device) in rows {
        archive_row(conn, &our_kind, their, device, now)?;
    }
    Ok(())
}

/// Every device we have a session row with for `their`, with its record, in device order.
pub(crate) fn session_devices(
    conn: &Connection,
    kind: IdentityKind,
    their: &str,
) -> Result<Vec<(DeviceId, SessionRecord)>, SignalProtocolError> {
    let rows: Vec<(i64, Vec<u8>)> = conn
        .prepare_cached(
            "SELECT device_id, record FROM sessions
             WHERE our_kind = ?1 AND their_service_id = ?2 ORDER BY device_id",
        )
        .and_then(|mut stmt| {
            stmt.query_map((kind.as_str(), their), |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        })
        .map_err(|e| sql_error("list sessions", &e))?;
    rows.into_iter()
        .filter_map(|(device, bytes)| {
            let device = DeviceId::try_from(u32::try_from(device).ok()?).ok()?;
            Some(SessionRecord::deserialize(&bytes).map(|record| (device, record)))
        })
        .collect()
}
