//! `DeviceStore`: the device row, upkeep and the resource report.
//!
//! The device is ONE row holding the JSON of `wacore::store::Device`, upstream's own persisted shape
//! (additive `#[serde(default)]` fields, so a newer wacore reads an older row); `format` names the
//! encoding so a later change can migrate it. The record holds the noise key, the identity key, the
//! signed prekey and the ADV secret, inside the SQLCipher file only. Wiped, as far as Rust lets us:
//! the buffer this crate serializes into is zeroized on drop. `key_pair_serde` and `serde_json`
//! make short-lived copies of their own that are not.
//!
//! `snapshot_db` keeps the trait's default no-op ON PURPOSE: a snapshot is a second copy of every key
//! of the account (upstream's is `VACUUM INTO` a plain file). Nothing here makes one.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use kourier_db::Priority;
use kourier_db::rusqlite::params;
use wacore::stats::StorageResourceReport;
use wacore::store::Device;
use wacore::store::error::Result;
use wacore::store::traits::DeviceStore;
use zeroize::Zeroizing;

use super::{KourierStore, blob, exec, first};
use crate::db::Points;
use crate::error::{database, unreadable};
use crate::failpoints;

/// `device.format`: 1 = the JSON of `wacore::store::Device`.
const FORMAT_JSON: i64 = 1;

/// The page cache cap of the connection (`cache_size = -8000`, KiB), which bounds what the report
/// calls memory.
const CACHE_BYTES: u64 = 8_000 * 1_024;

const SAVE: Points = Points {
    inside: failpoints::INSIDE_DEVICE_SAVE,
    after: failpoints::AFTER_DEVICE_SAVE,
};

/// The record of `device`, in a buffer that is wiped on drop and big enough for a device (a few
/// KiB) so the writer rarely reallocates and leaves a copy behind.
fn encode(device: &Device) -> Result<Zeroizing<Vec<u8>>> {
    let mut buffer = Zeroizing::new(Vec::with_capacity(8 * 1024));
    serde_json::to_writer(&mut *buffer, device)
        .map_err(|_| unreadable("the device could not be serialized"))?;
    Ok(buffer)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Our own two ids as the stored device names them, encoded (`pn:<digits>`, `lid:<digits>`): none, one or both, in that order.
/// Read inside the caller's zone by SQLite's own JSON functions, so that the device's keys never leave the engine and no
/// zone needs the actor's `OwnIds` handed down to it (W6a2 F13: the address book is no place for our own ids). The shape is
/// upstream's serde form of `Jid` (`user` is a field of it); a test pins it against a device saved the way the store saves it.
pub(crate) fn own_forms(conn: &kourier_db::rusqlite::Connection) -> Result<Vec<String>> {
    let ids = first(
        conn,
        "read our own ids",
        "SELECT json_extract(t, '$.pn.user'), json_extract(t, '$.lid.user')
         FROM (SELECT CAST(record AS TEXT) AS t FROM device WHERE id = 1 AND format = 1)",
        [],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )?;
    let (pn, lid) = ids.unwrap_or_default();
    Ok([("pn", pn), ("lid", lid)]
        .into_iter()
        .filter_map(|(scheme, user)| {
            kourier_model::UserId::from_parts(scheme, user.as_deref()?)
                .ok()
                .map(|id| id.as_str().to_owned())
        })
        .collect())
}

#[async_trait]
impl DeviceStore for KourierStore {
    async fn save(&self, device: &Device) -> Result<()> {
        const OP: &str = "save device failed";
        let record = encode(device)?;
        self.write_at(OP, SAVE, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO device(id, format, record, updated_at) VALUES (1, ?1, ?2, ?3) \
                 ON CONFLICT(id) DO UPDATE SET format = excluded.format, \
                 record = excluded.record, updated_at = excluded.updated_at",
                params![FORMAT_JSON, record.as_slice(), now_secs()],
            )?;
            Ok(())
        })
        .await
    }

    async fn load(&self) -> Result<Option<Device>> {
        const OP: &str = "load device failed";
        self.read(move |conn| {
            let row = first(
                conn,
                OP,
                "SELECT format, record FROM device WHERE id = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, Zeroizing::new(blob(row, 1)?))),
            )?;
            let Some((format, record)) = row else {
                return Ok(None);
            };
            if format != FORMAT_JSON {
                return Err(unreadable("the stored device has an unknown format"));
            }
            serde_json::from_slice::<Device>(&record)
                .map(Some)
                .map_err(|e| {
                    // Where it broke, never what was there: a serde error can quote a value.
                    tracing::error!(
                        line = e.line(),
                        column = e.column(),
                        "the stored device record could not be read"
                    );
                    unreadable("the stored device record could not be read")
                })
        })
        .await
    }

    async fn exists(&self) -> Result<bool> {
        const OP: &str = "device exists failed";
        self.read(move |conn| {
            Ok(
                first(conn, OP, "SELECT 1 FROM device WHERE id = 1", [], |row| {
                    row.get::<_, i64>(0)
                })?
                .is_some(),
            )
        })
        .await
    }

    /// One device per file: the row is made if it is not there, with a new device, and the id is
    /// always 1. A second call keeps the row (and so the keys) it finds.
    async fn create(&self) -> Result<i32> {
        const OP: &str = "create device failed";
        let record = encode(&Device::new())?;
        self.write_at(OP, SAVE, move |tx| {
            exec(
                tx,
                OP,
                "INSERT OR IGNORE INTO device(id, format, record, updated_at) VALUES (1, ?1, ?2, ?3)",
                params![FORMAT_JSON, record.as_slice(), now_secs()],
            )?;
            Ok(1)
        })
        .await
    }

    /// Page count and free list of the file; the memory is the page cache, which the connection
    /// caps. Best effort: any failure is "not reported" (all `None`), never a claim of zero.
    async fn resource_report(&self) -> StorageResourceReport {
        // SQLCipher answers `PRAGMA page_size` as TEXT (it is aliased to `cipher_page_size`), SQLite
        // as an integer: read either.
        let pragma = |conn: &kourier_db::rusqlite::Connection, name: &str| -> Option<u64> {
            use kourier_db::rusqlite::types::Value;
            let value: Value = conn
                .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                .ok()?;
            match value {
                Value::Integer(n) => u64::try_from(n).ok(),
                Value::Text(text) => text.trim().parse().ok(),
                _ => None,
            }
        };
        self.raw(Priority::Low, move |conn| {
            let (Some(size), Some(count), Some(free)) = (
                pragma(conn, "page_size"),
                pragma(conn, "page_count"),
                pragma(conn, "freelist_count"),
            ) else {
                return Ok(StorageResourceReport::default());
            };
            Ok(StorageResourceReport {
                memory_bytes: Some((count * size).min(CACHE_BYTES)),
                pages: Some(count),
                free_pages: Some(free),
                ..StorageResourceReport::default()
            })
        })
        .await
        .unwrap_or_default()
    }

    /// Upkeep on a coarse timer: the WAL is folded into the file (removed sessions and prekeys leave
    /// it; `secure_delete` is on). No `ANALYZE`: Kourier's files carry no planner statistics, and
    /// a read that needs an index names it.
    async fn maintenance(&self) -> Result<()> {
        self.raw(Priority::Low, move |conn| {
            kourier_db::try_checkpoint_truncate(conn)
                .map(drop)
                .map_err(|_| database("maintenance failed"))
        })
        .await
    }
}
