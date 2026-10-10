//! The forward-only migration runner (§5.7).
//!
//! Each component (the core, each backend, `core_outbox`) has a numbered list of steps, usually
//! `migrations/NNNN_name.sql` embedded with [`migration!`](crate::migration). All pending steps of
//! a component run in one `BEGIN IMMEDIATE` transaction at open and are recorded in
//! `schema_version(component, version)`. A file whose recorded version is higher than the
//! binary's latest step is refused, never downgraded.

use crate::{Error, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

/// What one migration step does.
#[derive(Clone, Copy)]
pub enum Step {
    /// A batch of SQL statements (usually an embedded `.sql` file).
    Sql(&'static str),
    /// A Rust-side data migration (rare), run inside the same transaction.
    Rust(fn(&Transaction<'_>) -> rusqlite::Result<()>),
    /// SQL that rebuilds a table other tables reference (SQLite's "make other kinds of table
    /// schema changes" procedure: create the new table, copy, drop the old one, rename). A
    /// `DROP TABLE` with foreign keys enforced would run the children's `ON DELETE` actions, so
    /// the whole run of a component with such a pending step happens with `foreign_keys = OFF`;
    /// `PRAGMA foreign_key_check` must come back empty before the commit, else the run rolls
    /// back. Enforcement is restored afterwards on every path.
    Rebuild(&'static str),
}

/// One numbered step of a component's schema.
#[derive(Clone, Copy)]
pub struct Migration {
    /// Step number, starting at 1 and contiguous.
    pub version: u32,
    /// `NNNN_name`; a leading number must equal `version`.
    pub name: &'static str,
    /// The work.
    pub step: Step,
}

/// The ordered steps of one schema component.
#[derive(Clone, Copy)]
pub struct Migrations {
    /// Key in `schema_version` (`core`, `signal`, `core_outbox`, …).
    pub component: &'static str,
    /// Steps 1..=n in order.
    pub steps: &'static [Migration],
}

impl Migrations {
    /// A component's steps.
    pub const fn new(component: &'static str, steps: &'static [Migration]) -> Self {
        Self { component, steps }
    }

    /// The latest version this binary knows (0 for an empty list).
    pub fn latest(&self) -> u32 {
        self.steps.last().map_or(0, |m| m.version)
    }

    fn validate(&self) -> Result<()> {
        let invalid = |reason: String| Error::InvalidMigrations {
            component: self.component,
            reason,
        };
        if self.component.is_empty() {
            return Err(invalid("empty component name".into()));
        }
        for (i, m) in self.steps.iter().enumerate() {
            let expected = u32::try_from(i + 1).map_err(|_| invalid("too many steps".into()))?;
            if m.version != expected {
                return Err(invalid(format!(
                    "step {} ({}) is out of order, expected {expected}",
                    m.version, m.name
                )));
            }
            let digits: String = m.name.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() && digits.parse::<u32>().ok() != Some(m.version) {
                return Err(invalid(format!(
                    "file name {} does not match step {}",
                    m.name, m.version
                )));
            }
        }
        Ok(())
    }
}

/// Embeds `migrations/<name>.sql` of the calling crate as step `version`:
/// `kourier_db::migration!(1, "0001_init")`.
#[macro_export]
macro_rules! migration {
    ($version:expr, $name:literal) => {
        $crate::Migration {
            version: $version,
            name: $name,
            step: $crate::Step::Sql(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/migrations/",
                $name,
                ".sql"
            ))),
        }
    };
}

/// Like [`migration!`], for a [`Step::Rebuild`] step:
/// `kourier_db::migration_rebuild!(3, "0003_message_ids")`.
#[macro_export]
macro_rules! migration_rebuild {
    ($version:expr, $name:literal) => {
        $crate::Migration {
            version: $version,
            name: $name,
            step: $crate::Step::Rebuild(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/migrations/",
                $name,
                ".sql"
            ))),
        }
    };
}

/// The result of [`migrate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// Version found in the file (0 for a new component).
    pub from: u32,
    /// Version after the run.
    pub to: u32,
}

/// Brings `component` up to its latest step, in one transaction. Refuses a newer file with
/// [`Error::TooNew`]; a failing step rolls back the whole run ([`Error::Migration`]).
pub fn migrate(conn: &mut Connection, migrations: &Migrations) -> Result<MigrationOutcome> {
    migrations.validate()?;
    // A pending table rebuild runs the whole run without foreign-key enforcement (the pragma
    // is a no-op inside a transaction, so it is switched before `BEGIN`).
    let found = schema_version(conn, migrations.component)?.unwrap_or(0);
    let rebuild = migrations
        .steps
        .iter()
        .any(|m| i64::from(m.version) > found && matches!(m.step, Step::Rebuild(_)));
    let enforced: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    if !(rebuild && enforced) {
        return run(conn, migrations, false);
    }
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    let out = run(conn, migrations, true);
    let restored = conn.execute_batch("PRAGMA foreign_keys = ON");
    let out = out?;
    restored?;
    Ok(out)
}

fn run(
    conn: &mut Connection,
    migrations: &Migrations,
    check_keys: bool,
) -> Result<MigrationOutcome> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version(
           component TEXT PRIMARY KEY, version INTEGER NOT NULL)",
    )?;
    let found = read_version(&tx, migrations.component)?.unwrap_or(0);
    let latest = i64::from(migrations.latest());
    if found > latest {
        return Err(Error::TooNew {
            component: migrations.component.to_owned(),
            found,
            supported: latest,
        });
    }
    let from = u32::try_from(found).map_err(|_| Error::InvalidMigrations {
        component: migrations.component,
        reason: format!("recorded version {found} is negative"),
    })?;
    for m in migrations.steps.iter().filter(|m| m.version > from) {
        let applied = match m.step {
            Step::Sql(sql) | Step::Rebuild(sql) => tx.execute_batch(sql),
            Step::Rust(f) => f(&tx),
        };
        applied.map_err(|e| Error::Migration {
            component: migrations.component,
            version: m.version,
            name: m.name,
            source: Box::new(e.into()),
        })?;
    }
    if check_keys {
        let broken: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })?;
        if broken > 0 {
            let step = migrations
                .steps
                .iter()
                .rev()
                .find(|m| matches!(m.step, Step::Rebuild(_)));
            return Err(Error::Migration {
                component: migrations.component,
                version: step.map_or(migrations.latest(), |m| m.version),
                name: step.map_or("", |m| m.name),
                source: Box::new(Error::ForeignKeys(broken)),
            });
        }
    }
    let to = migrations.latest().max(from);
    if to != from {
        tx.execute(
            "INSERT INTO schema_version(component, version) VALUES (?1, ?2)
             ON CONFLICT(component) DO UPDATE SET version = excluded.version",
            (migrations.component, to),
        )?;
        tracing::info!(
            component = migrations.component,
            from,
            to,
            "schema migrated"
        );
    }
    tx.commit()?;
    Ok(MigrationOutcome { from, to })
}

/// The recorded version of `component`, `None` when it was never migrated.
pub fn schema_version(conn: &Connection, component: &str) -> Result<Option<i64>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    read_version(conn, component)
}

fn read_version(conn: &Connection, component: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT version FROM schema_version WHERE component = ?1",
            [component],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DbKey;

    const V1: &[Migration] = &[Migration {
        version: 1,
        name: "0001_init",
        step: Step::Sql("CREATE TABLE a(x INTEGER NOT NULL);"),
    }];

    fn fill(tx: &Transaction<'_>) -> rusqlite::Result<()> {
        // A Rust data step: derive rows from the previous schema.
        tx.execute("INSERT INTO b(y) SELECT x * 2 FROM a", [])?;
        Ok(())
    }

    const V3: &[Migration] = &[
        V1[0],
        Migration {
            version: 2,
            name: "0002_b",
            step: Step::Sql("CREATE TABLE b(y INTEGER NOT NULL);"),
        },
        Migration {
            version: 3,
            name: "0003_fill_b",
            step: Step::Rust(fill),
        },
    ];

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::open(&dir.path().join("m.db"), &DbKey::from([5; 32])).unwrap();
        (dir, conn)
    }

    #[test]
    fn fresh_then_upgrade_then_noop() {
        let (_dir, mut conn) = db();
        let out = migrate(&mut conn, &Migrations::new("test", V1)).unwrap();
        assert_eq!(out, MigrationOutcome { from: 0, to: 1 });
        conn.execute("INSERT INTO a VALUES (21)", []).unwrap();

        // The next release adds steps 2 and 3; data from release 1 survives and is migrated.
        let out = migrate(&mut conn, &Migrations::new("test", V3)).unwrap();
        assert_eq!(out, MigrationOutcome { from: 1, to: 3 });
        let y: i64 = conn.query_row("SELECT y FROM b", [], |r| r.get(0)).unwrap();
        assert_eq!(y, 42);

        let out = migrate(&mut conn, &Migrations::new("test", V3)).unwrap();
        assert_eq!(out, MigrationOutcome { from: 3, to: 3 });
        assert_eq!(schema_version(&conn, "test").unwrap(), Some(3));
        assert_eq!(schema_version(&conn, "other").unwrap(), None);
    }

    #[test]
    fn components_are_independent() {
        let (_dir, mut conn) = db();
        migrate(&mut conn, &Migrations::new("one", V1)).unwrap();
        let other: &[Migration] = &[Migration {
            version: 1,
            name: "0001_c",
            step: Step::Sql("CREATE TABLE c(z);"),
        }];
        migrate(&mut conn, &Migrations::new("two", other)).unwrap();
        assert_eq!(schema_version(&conn, "one").unwrap(), Some(1));
        assert_eq!(schema_version(&conn, "two").unwrap(), Some(1));
    }

    #[test]
    fn newer_database_is_refused_unchanged() {
        let (_dir, mut conn) = db();
        migrate(&mut conn, &Migrations::new("test", V3)).unwrap();
        let err = migrate(&mut conn, &Migrations::new("test", V1)).unwrap_err();
        assert!(
            err.to_string().contains("newer than this build supports"),
            "{err}"
        );
        match err {
            Error::TooNew {
                component,
                found,
                supported,
            } => assert_eq!((component.as_str(), found, supported), ("test", 3, 1)),
            other => panic!("unexpected {other}"),
        }
        assert_eq!(schema_version(&conn, "test").unwrap(), Some(3));
    }

    #[test]
    fn failing_step_rolls_back_the_whole_run() {
        let (_dir, mut conn) = db();
        const BROKEN: &[Migration] = &[
            V1[0],
            Migration {
                version: 2,
                name: "0002_broken",
                step: Step::Sql("CREATE TABLE b(y); INSERT INTO nowhere VALUES (1);"),
            },
        ];
        let err = migrate(&mut conn, &Migrations::new("test", BROKEN)).unwrap_err();
        assert!(matches!(err, Error::Migration { version: 2, .. }), "{err}");
        assert_eq!(schema_version(&conn, "test").unwrap(), None);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('a', 'b')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "step 1 was rolled back with step 2");
    }

    const PARENT: &[Migration] = &[Migration {
        version: 1,
        name: "0001_parent",
        step: Step::Sql(
            "CREATE TABLE p(id INTEGER PRIMARY KEY, v TEXT);
             CREATE TABLE c(p_id INTEGER NOT NULL REFERENCES p ON DELETE CASCADE, w TEXT);",
        ),
    }];

    /// The parent gets AUTOINCREMENT through a rebuild: a plain `DROP TABLE p` with foreign keys
    /// enforced would have cascaded into `c`.
    const REBUILT: &[Migration] = &[
        PARENT[0],
        Migration {
            version: 2,
            name: "0002_rebuild",
            step: Step::Rebuild(
                "CREATE TABLE p_v2(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
                 INSERT INTO p_v2(id, v) SELECT id, v FROM p ORDER BY id;
                 DROP TABLE p;
                 ALTER TABLE p_v2 RENAME TO p;",
            ),
        },
    ];

    #[test]
    fn rebuild_keeps_children_and_their_foreign_keys() {
        let (_dir, mut conn) = db();
        migrate(&mut conn, &Migrations::new("test", PARENT)).unwrap();
        conn.execute_batch(
            "INSERT INTO p(id, v) VALUES (1, 'a'), (2, 'b');
             INSERT INTO c(p_id, w) VALUES (1, 'x'), (2, 'y');",
        )
        .unwrap();
        let out = migrate(&mut conn, &Migrations::new("test", REBUILT)).unwrap();
        assert_eq!(out, MigrationOutcome { from: 1, to: 2 });
        let children: i64 = conn
            .query_row("SELECT count(*) FROM c", [], |r| r.get(0))
            .unwrap();
        assert_eq!(children, 2, "the drop of the old parent cascaded nothing");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "enforcement is back on");
        // The children reference the new table: the cascade still works, and ids are never
        // handed out twice.
        conn.execute("DELETE FROM p WHERE id = 2", []).unwrap();
        let children: i64 = conn
            .query_row("SELECT count(*) FROM c", [], |r| r.get(0))
            .unwrap();
        assert_eq!(children, 1);
        conn.execute("INSERT INTO p(v) VALUES ('c')", []).unwrap();
        assert_eq!(conn.last_insert_rowid(), 3, "id 2 is not reused");
        let sql: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'c'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(sql.contains("REFERENCES p "), "{sql}");
    }

    #[test]
    fn rebuild_with_dangling_keys_rolls_back() {
        let (_dir, mut conn) = db();
        migrate(&mut conn, &Migrations::new("test", PARENT)).unwrap();
        conn.execute_batch(
            "INSERT INTO p(id, v) VALUES (1, 'a');
             INSERT INTO c(p_id, w) VALUES (1, 'x');",
        )
        .unwrap();
        const LOSSY: &[Migration] = &[
            PARENT[0],
            Migration {
                version: 2,
                name: "0002_lossy",
                step: Step::Rebuild(
                    "CREATE TABLE p_v2(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT);
                     DROP TABLE p;
                     ALTER TABLE p_v2 RENAME TO p;",
                ),
            },
        ];
        let err = migrate(&mut conn, &Migrations::new("test", LOSSY)).unwrap_err();
        assert!(
            matches!(&err, Error::Migration { version: 2, source, .. }
                if matches!(**source, Error::ForeignKeys(1))),
            "{err}"
        );
        assert_eq!(schema_version(&conn, "test").unwrap(), Some(1));
        let parents: i64 = conn
            .query_row("SELECT count(*) FROM p", [], |r| r.get(0))
            .unwrap();
        assert_eq!(parents, 1, "rolled back");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "enforcement is back on after a failure");
    }

    #[test]
    fn malformed_lists_are_rejected() {
        let (_dir, mut conn) = db();
        let gap: &[Migration] = &[Migration {
            version: 2,
            name: "0002_x",
            step: Step::Sql(""),
        }];
        let misnamed: &[Migration] = &[Migration {
            version: 1,
            name: "0007_x",
            step: Step::Sql(""),
        }];
        for steps in [gap, misnamed] {
            assert!(matches!(
                migrate(&mut conn, &Migrations::new("test", steps)),
                Err(Error::InvalidMigrations { .. })
            ));
        }
    }
}
