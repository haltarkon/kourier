//! The Telegram `backend.db` (TG-1 plan §2.8): opening it through `kourier-db` in WhatsApp's order
//! (refuse a foreign file BEFORE any migration), its migrations, and the account row.

use std::path::Path;

use kourier_db::rusqlite::{Connection, OptionalExtension, params};
use kourier_db::{DbKey, Migrations, migration};

use crate::error::{OpenError, StoreError, sql};
use crate::parts::TgEnv;

/// The Telegram schema component (`schema_version.component = 'telegram'`). Migrations are frozen
/// once they ran on a kept run profile.
pub(crate) const MIGRATIONS: Migrations = Migrations::new(
    "telegram",
    &[migration!(1, "0001_init"), migration!(2, "0002_chats")],
);

/// Opens (creating if needed) `backend.db` with its key and brings both schema components
/// (`core_outbox` from kourier-db, then `telegram`) up to date. A wrong key fails before anything
/// is written. A file that carries any component other than those two is refused as
/// `OtherNetwork` before any migration: its bytes are not touched.
pub(crate) fn open(path: &Path, key: &DbKey) -> Result<Connection, OpenError> {
    let mut conn = kourier_db::open(path, key)?;
    kourier_db::check_backend_file(&conn, &MIGRATIONS)?;
    kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS)?;
    kourier_db::migrate(&mut conn, &MIGRATIONS)?;
    Ok(conn)
}

/// The one account row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountRow {
    /// Production or Telegram's test servers; binding once written.
    pub(crate) env: TgEnv,
    /// TDLib's id of the own user, once the account is ready.
    pub(crate) user_id: Option<i64>,
    /// The TDLib commit that created the account's `tdlib/` directory.
    pub(crate) pin: String,
    /// When the row was written (ms since the epoch).
    pub(crate) created_ms: i64,
}

/// The account row, if one was written.
pub(crate) fn account(conn: &Connection) -> Result<Option<AccountRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT env, user_id, pin, created_ms FROM account WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sql("read account"))?;
    let Some((env, user_id, pin, created_ms)) = row else {
        return Ok(None);
    };
    let env = TgEnv::parse(&env).ok_or(StoreError("account env unreadable"))?;
    Ok(Some(AccountRow {
        env,
        user_id,
        pin,
        created_ms,
    }))
}

/// Writes the account row; refused when one exists (the environment binds once). The first
/// `StartLogin` of a `Fresh` actor writes it (`actor::Actor::start_login`) before the helper starts.
pub(crate) fn create_account(conn: &Connection, row: &AccountRow) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO account(id, env, user_id, pin, created_ms) VALUES (1, ?1, ?2, ?3, ?4)",
        params![row.env.as_str(), row.user_id, row.pin, row.created_ms],
    )
    .map_err(sql("create account"))?;
    Ok(())
}

/// Sets the account's TDLib user id in the CALLER's transaction (the one that appends
/// `AccountReady`, `outbox::append_ready`): refused outside a transaction, without the account
/// row, and when the row already holds ANOTHER user id (one identity per account; the login's
/// identity checks ran before). Setting the same id again is a no-op that succeeds (a relink).
pub(crate) fn set_user_id(conn: &Connection, user_id: i64) -> Result<(), StoreError> {
    if conn.is_autocommit() {
        return Err(StoreError("set user id outside a transaction"));
    }
    let changed = conn
        .execute(
            "UPDATE account SET user_id = ?1 WHERE id = 1 AND (user_id IS NULL OR user_id = ?1)",
            params![user_id],
        )
        .map_err(sql("set user id"))?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError("set user id: no account row, or another user"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use kourier_db::DbKey;
    use kourier_db::rusqlite::params;
    use sha2::{Digest, Sha256};

    use super::*;

    fn key() -> DbKey {
        DbKey::from_bytes([7; 32])
    }

    fn path(dir: &Path) -> PathBuf {
        dir.join("backend.db")
    }

    /// The hash of every file in the directory (`backend.db`, `-wal`, `-shm`), by name.
    fn files_hash(dir: &Path) -> String {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").path())
            .collect();
        names.sort();
        let mut hasher = Sha256::new();
        for p in names {
            hasher.update(p.file_name().expect("name").as_encoded_bytes());
            hasher.update(std::fs::read(&p).expect("read"));
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn version(conn: &Connection, component: &str) -> Option<i64> {
        conn.query_row(
            "SELECT version FROM schema_version WHERE component = ?1",
            params![component],
            |r| r.get(0),
        )
        .optional()
        .expect("schema_version")
    }

    #[test]
    fn opened_twice_is_one_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        for _ in 0..2 {
            drop(open(&path(dir.path()), &key()).expect("open"));
        }
        let conn = kourier_db::open(&path(dir.path()), &key()).expect("raw open");
        let outbox = i64::from(kourier_db::outbox::MIGRATIONS.latest());
        assert_eq!(
            version(&conn, "telegram"),
            Some(i64::from(MIGRATIONS.latest())),
            "telegram version"
        );
        assert_eq!(
            version(&conn, "core_outbox"),
            Some(outbox),
            "outbox version"
        );
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM schema_version", [], |r| r.get(0))
            .expect("count");
        assert_eq!(
            rows, 2,
            "a component was written twice or a third one exists"
        );
        assert_eq!(
            account(&conn).expect("read"),
            None,
            "a fresh file has an account"
        );
    }

    #[test]
    fn another_networks_file_is_refused_and_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut conn = kourier_db::open(&path(dir.path()), &key()).expect("raw open");
            kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).expect("outbox");
            conn.execute(
                "INSERT INTO schema_version(component, version) VALUES ('whatsapp', 1)",
                [],
            )
            .expect("foreign row");
        }
        let before = files_hash(dir.path());
        let err = open(&path(dir.path()), &key()).expect_err("a WhatsApp file opened");
        assert!(
            matches!(err, OpenError::OtherNetwork),
            "not OtherNetwork: {err}"
        );
        assert_eq!(files_hash(dir.path()), before, "the foreign file changed");
    }

    #[test]
    fn a_wrong_key_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        drop(open(&path(dir.path()), &key()).expect("open"));
        let before = files_hash(dir.path());
        let err = open(&path(dir.path()), &DbKey::from_bytes([8; 32])).expect_err("wrong key");
        assert!(matches!(err, OpenError::WrongKey), "not WrongKey: {err}");
        assert_eq!(
            files_hash(dir.path()),
            before,
            "a wrong key changed the file"
        );
    }

    #[test]
    fn the_account_row_is_written_once_and_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open(&path(dir.path()), &key()).expect("open");
        let row = AccountRow {
            env: TgEnv::Test,
            user_id: None,
            pin: crate::PIN.commit.to_owned(),
            created_ms: 1_700_000_000_000,
        };
        create_account(&conn, &row).expect("create");
        assert_eq!(account(&conn).expect("read"), Some(row.clone()));
        let again = AccountRow {
            env: TgEnv::Production,
            ..row.clone()
        };
        assert!(
            create_account(&conn, &again).is_err(),
            "the env was written twice"
        );
        assert_eq!(account(&conn).expect("read"), Some(row), "the row changed");
        assert!(
            conn.execute("UPDATE account SET env = 'staging' WHERE id = 1", [])
                .is_err(),
            "the schema accepts an unknown env"
        );
    }

    #[test]
    fn the_user_id_is_set_once_and_only_inside_a_transaction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = open(&path(dir.path()), &key()).expect("open");
        {
            let tx = conn.unchecked_transaction().expect("tx");
            assert!(set_user_id(&tx, 100).is_err(), "set without a row");
        }
        create_account(
            &conn,
            &AccountRow {
                env: TgEnv::Test,
                user_id: None,
                pin: crate::PIN.commit.to_owned(),
                created_ms: 1,
            },
        )
        .expect("create");
        assert!(
            set_user_id(&conn, 100).is_err(),
            "set outside a transaction"
        );
        let user = |conn: &Connection| account(conn).expect("read").and_then(|r| r.user_id);
        assert_eq!(user(&conn), None);
        let tx = conn.unchecked_transaction().expect("tx");
        set_user_id(&tx, 100).expect("set");
        set_user_id(&tx, 100).expect("the same id again");
        assert!(set_user_id(&tx, 101).is_err(), "another user replaced it");
        tx.commit().expect("commit");
        assert_eq!(user(&conn), Some(100));
    }
}
