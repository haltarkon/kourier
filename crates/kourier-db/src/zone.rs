//! The synchronous zone API (§6.3).
//!
//! A zone is one `BEGIN IMMEDIATE … COMMIT` around a synchronous closure. Because the closure
//! cannot await, no other task on the same thread can run SQL on the connection while the
//! transaction is open, and no network I/O can happen inside it (§6.3 rules 1-2). Writes to
//! in-memory caches are registered with [`Tx::on_commit`] and applied only after the commit, so a
//! rolled-back zone leaves caches untouched (Desktop's zone overlay).

use crate::{Error, Result, outbox};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use std::cell::{Cell, RefCell};
use std::ops::Deref;

/// A connection owned by one thread (a backend actor or the core writer) whose every access
/// goes through [`ZoneDb::zone`] or [`ZoneDb::read`]. `Send`, not `Sync`: share it as
/// `Rc<ZoneDb>` between `spawn_local` tasks.
pub struct ZoneDb {
    conn: RefCell<Connection>,
    in_zone: Cell<bool>,
}

/// The transaction handed to a zone body. Derefs to [`Connection`] for queries.
pub struct Tx<'c> {
    tx: Transaction<'c>,
    on_commit: Vec<Box<dyn FnOnce()>>,
}

impl Tx<'_> {
    /// Runs `f` after the zone committed, once the connection is released (so `f` may open
    /// the next zone); dropped unrun when the zone rolls back.
    pub fn on_commit(&mut self, f: impl FnOnce() + 'static) {
        self.on_commit.push(Box::new(f));
    }

    /// Appends one opaque, versioned envelope to `core_outbox` in this transaction (§4.1
    /// principle 6) and returns its `seq`.
    pub fn outbox_append(&self, envelope: &[u8], created_at: i64) -> Result<i64> {
        outbox::append(&self.tx, envelope, created_at)
    }
}

impl Deref for Tx<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.tx
    }
}

impl ZoneDb {
    /// Takes ownership of an open connection (see [`crate::open()`]).
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: RefCell::new(conn),
            in_zone: Cell::new(false),
        }
    }

    /// Runs `f` in one `BEGIN IMMEDIATE` transaction: commit when it returns `Ok`, roll back when
    /// it returns `Err` (or panics). [`Tx::on_commit`] callbacks run after a successful commit.
    /// Zones never nest: a debug build panics, a release build returns [`Error::NestedZone`].
    pub fn zone<R, E>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<R, E>) -> Result<R, E>
    where
        E: From<Error>,
    {
        let (out, on_commit) = {
            let _guard = self.enter()?;
            let mut conn = self.conn.try_borrow_mut().map_err(|_| Error::NestedZone)?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(Error::from)?;
            let mut tx = Tx {
                tx,
                on_commit: Vec::new(),
            };
            let out = f(&mut tx)?;
            let Tx { tx, on_commit } = tx;
            tx.commit().map_err(Error::from)?;
            (out, on_commit)
        };
        for f in on_commit {
            f();
        }
        Ok(out)
    }

    /// A zone without writes: `f` sees one consistent snapshot (a deferred transaction) and the
    /// connection is `query_only` meanwhile, so a write inside fails.
    pub fn read<R, E>(&self, f: impl FnOnce(&Connection) -> Result<R, E>) -> Result<R, E>
    where
        E: From<Error>,
    {
        let _guard = self.enter()?;
        let conn = self.conn.try_borrow_mut().map_err(|_| Error::NestedZone)?;
        conn.execute_batch("PRAGMA query_only = ON")
            .map_err(Error::from)?;
        // Lifts query_only on every exit path, a panicking `f` included; declared before the
        // transaction, so the transaction ends (rolls back) first.
        let query_only = QueryOnly(Some(&*conn));
        let tx = conn.unchecked_transaction().map_err(Error::from)?;
        let out = f(&tx)?;
        tx.commit().map_err(Error::from)?;
        query_only.lift()?;
        Ok(out)
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)` between zones ([`crate::try_checkpoint_truncate`]):
    /// `true` when the WAL file is empty afterwards, `false` when a reader held it back. For
    /// a caller that just removed a secret and wants no earlier version of its page left in
    /// the WAL. Inside a zone it fails like a nested zone.
    pub fn checkpoint_truncate(&self) -> Result<bool> {
        let _guard = self.enter()?;
        let conn = self.conn.try_borrow_mut().map_err(|_| Error::NestedZone)?;
        crate::try_checkpoint_truncate(&conn)
    }

    /// Gives the connection back (for shutdown and tests).
    pub fn into_inner(self) -> Connection {
        self.conn.into_inner()
    }

    fn enter(&self) -> Result<ZoneGuard<'_>, Error> {
        debug_assert!(!self.in_zone.get(), "zones never nest (§6.3 rule 1)");
        if self.in_zone.replace(true) {
            return Err(Error::NestedZone);
        }
        Ok(ZoneGuard(&self.in_zone))
    }
}

/// Clears the "inside a zone" flag on every exit path, panics included.
struct ZoneGuard<'a>(&'a Cell<bool>);

impl Drop for ZoneGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Turns `query_only` off again when a [`ZoneDb::read`] ends.
struct QueryOnly<'a>(Option<&'a Connection>);

impl QueryOnly<'_> {
    /// The normal exit: lift it and report a failure.
    fn lift(mut self) -> Result<()> {
        match self.0.take() {
            Some(conn) => Ok(conn.execute_batch("PRAGMA query_only = OFF")?),
            None => Ok(()),
        }
    }
}

impl Drop for QueryOnly<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.0.take() {
            let _ = conn.execute_batch("PRAGMA query_only = OFF");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbKey, migrate};
    use std::rc::Rc;

    fn db() -> (tempfile::TempDir, ZoneDb) {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::open(&dir.path().join("z.db"), &DbKey::from([9; 32])).unwrap();
        migrate(&mut conn, &outbox::MIGRATIONS).unwrap();
        conn.execute_batch("CREATE TABLE state(k TEXT PRIMARY KEY, v INTEGER NOT NULL)")
            .unwrap();
        (dir, ZoneDb::new(conn))
    }

    fn count(db: &ZoneDb, sql: &str) -> i64 {
        db.read(|c| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) })
            .unwrap()
    }

    #[test]
    fn commit_applies_state_events_and_overlay_together() {
        let (_dir, db) = db();
        let cache = Rc::new(Cell::new(0));
        let seq = db
            .zone(|tx| -> Result<i64> {
                tx.execute("INSERT INTO state VALUES ('ratchet', 1)", [])?;
                let c = cache.clone();
                tx.on_commit(move || c.set(1));
                tx.outbox_append(b"v1:event", 0)
            })
            .unwrap();
        assert_eq!(seq, 1);
        assert_eq!(cache.get(), 1);
        assert_eq!(count(&db, "SELECT count(*) FROM state"), 1);
        assert_eq!(count(&db, "SELECT count(*) FROM core_outbox"), 1);
    }

    #[test]
    fn rollback_drops_state_events_and_overlay_together() {
        let (_dir, db) = db();
        let cache = Rc::new(Cell::new(0));
        let err = db
            .zone(|tx| -> Result<()> {
                tx.execute("INSERT INTO state VALUES ('ratchet', 1)", [])?;
                tx.outbox_append(b"v1:event", 0)?;
                let c = cache.clone();
                tx.on_commit(move || c.set(1));
                Err(Error::WrongKey)
            })
            .unwrap_err();
        assert!(matches!(err, Error::WrongKey));
        assert_eq!(cache.get(), 0, "overlay never applied");
        assert_eq!(count(&db, "SELECT count(*) FROM state"), 0);
        assert_eq!(count(&db, "SELECT count(*) FROM core_outbox"), 0);
        // The connection is usable again.
        db.zone(|tx| -> Result<usize> { Ok(tx.execute("INSERT INTO state VALUES ('x', 2)", [])?) })
            .unwrap();
    }

    #[test]
    fn read_rejects_writes() {
        let (_dir, db) = db();
        let res = db
            .read(|c| -> Result<usize> { Ok(c.execute("INSERT INTO state VALUES ('x', 1)", [])?) });
        assert!(res.is_err());
        assert_eq!(count(&db, "SELECT count(*) FROM state"), 0);
        // query_only is lifted again for zones.
        db.zone(|tx| -> Result<usize> { Ok(tx.execute("INSERT INTO state VALUES ('x', 1)", [])?) })
            .unwrap();
    }

    #[test]
    fn a_panicking_read_leaves_the_connection_writable() {
        let (_dir, db) = db();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = db.read(|_| -> Result<()> { panic!("query bug") });
        }));
        assert!(caught.is_err());
        db.zone(|tx| -> Result<usize> { Ok(tx.execute("INSERT INTO state VALUES ('x', 1)", [])?) })
            .unwrap();
        assert_eq!(count(&db, "SELECT count(*) FROM state"), 1);
    }

    #[test]
    fn on_commit_runs_after_the_connection_is_released() {
        let (_dir, db) = db();
        let db = Rc::new(db);
        let seen = Rc::new(Cell::new(-1));
        let (hook_db, hook_seen) = (db.clone(), seen.clone());
        db.zone(|tx| -> Result<()> {
            tx.execute("INSERT INTO state VALUES ('k', 7)", [])?;
            tx.on_commit(move || {
                // A cache refresh may read the committed state right away.
                let v = hook_db
                    .read(|c| -> Result<i64> {
                        Ok(c.query_row("SELECT v FROM state WHERE k = 'k'", [], |r| r.get(0))?)
                    })
                    .unwrap();
                hook_seen.set(v);
            });
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.get(), 7);
    }

    /// A checkpoint between zones empties the WAL file; one that a reader with an open
    /// snapshot holds back says so, and succeeds once the reader is gone.
    #[test]
    fn a_checkpoint_between_zones_empties_the_wal_unless_a_reader_holds_it() {
        let (dir, db) = db();
        let wal = dir.path().join("z.db-wal");
        db.zone(|tx| -> Result<usize> { Ok(tx.execute("INSERT INTO state VALUES ('a', 1)", [])?) })
            .unwrap();
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        assert!(db.checkpoint_truncate().unwrap());
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);

        // A reader in the middle of a snapshot of an older state of the WAL.
        db.zone(|tx| -> Result<usize> { Ok(tx.execute("INSERT INTO state VALUES ('b', 2)", [])?) })
            .unwrap();
        let reader =
            crate::open_read_only(&dir.path().join("z.db"), &DbKey::from([9; 32])).unwrap();
        reader.busy_timeout(std::time::Duration::ZERO).unwrap();
        let snapshot = reader.unchecked_transaction().unwrap();
        let _: i64 = snapshot
            .query_row("SELECT count(*) FROM state", [], |r| r.get(0))
            .unwrap();
        db.zone(|tx| -> Result<usize> { Ok(tx.execute("INSERT INTO state VALUES ('c', 3)", [])?) })
            .unwrap();
        // The writer's own wait for readers is cut short for the test.
        db.zone(|tx| -> Result<()> { Ok(tx.busy_timeout(std::time::Duration::from_millis(50))?) })
            .unwrap();
        assert!(
            !db.checkpoint_truncate().unwrap(),
            "held back by the reader"
        );
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        drop(snapshot);
        assert!(db.checkpoint_truncate().unwrap());
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "zones never nest"))]
    fn zones_never_nest() {
        let (_dir, db) = db();
        let inner = db.zone(|_tx| -> Result<Result<()>> { Ok(db.zone(|_| Ok(()))) });
        // Release builds: the inner zone fails, the outer one commits.
        assert!(matches!(inner, Ok(Err(Error::NestedZone))));
    }
}
