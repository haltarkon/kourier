//! The WhatsApp `backend.db` (architecture §5.5, §7): opening it through `kourier-db`, its
//! migrations, and the transaction helper every write goes through.

use std::path::Path;

use kourier_db::rusqlite::{Connection, Transaction, TransactionBehavior};
use kourier_db::{DbKey, Migrations, failpoint, migration};
use wacore::store::error::StoreError;

use crate::error::{OpenError, sql};

/// The WhatsApp schema component (`schema_version.component = 'whatsapp'`). Its numbers are this
/// crate's own: they never meet Signal's (22 today) because each network has its own file.
pub(crate) const MIGRATIONS: Migrations = Migrations::new(
    "whatsapp",
    &[
        migration!(1, "0001_init"),
        migration!(2, "0002_inbox"),
        migration!(3, "0003_inbox_age"),
        migration!(4, "0004_history"),
        migration!(5, "0005_send"),
        migration!(6, "0006_media"),
        migration!(7, "0007_chat_state"),
        migration!(8, "0008_people"),
    ],
);

/// Opens (creating if needed) `backend.db` with its key and brings both schema components
/// (`core_outbox` from kourier-db, then `whatsapp`) up to date. A wrong key fails before anything is
/// written. A file that carries any component other than those two (another network's backend, the
/// core, one that does not exist yet) is read first and refused as `OtherNetwork`, before any
/// migration: its bytes are not touched.
pub(crate) fn open(path: &Path, key: &DbKey) -> Result<Connection, OpenError> {
    let mut conn = kourier_db::open(path, key)?;
    kourier_db::check_backend_file(&conn, &MIGRATIONS)?;
    kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS)?;
    kourier_db::migrate(&mut conn, &MIGRATIONS)?;
    Ok(conn)
}

/// The kill points around the commit of one operation (`KOURIER_FAILPOINT=<name>[:<n>]`): `inside`
/// is hit with everything written and nothing committed, `after` right after the commit.
#[derive(Clone, Copy)]
pub(crate) struct Points {
    pub(crate) inside: &'static str,
    pub(crate) after: &'static str,
}

/// One `BEGIN IMMEDIATE` transaction around `f`: the kill point `inside`, `COMMIT`, the kill point
/// `after`. An error from `f` rolls everything back (the transaction is dropped); `Ok` is returned
/// only after the commit.
pub(crate) fn zone<T>(
    conn: &mut Connection,
    op: &'static str,
    points: Option<Points>,
    f: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql(op))?;
    let out = f(&tx)?;
    if let Some(points) = points {
        failpoint::hit(points.inside);
    }
    tx.commit().map_err(sql(op))?;
    if let Some(points) = points {
        failpoint::hit(points.after);
    }
    Ok(out)
}
