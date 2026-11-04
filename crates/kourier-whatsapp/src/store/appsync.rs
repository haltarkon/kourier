//! `AppSyncStore`: app-state sync keys, the version of each collection and the mutation MACs.
//!
//! The atomic unit is [`AppSyncStore::commit_patch`]: the collection's new version, the index MACs
//! the patch removed and the MACs it added become visible together, in ONE job and ONE
//! transaction (removals first, because an index in both ends with the added value). The same
//! transaction writes Kourier's own `appstate_dirty(name, version)` (architecture §7.1 rule (b)):
//! a collection whose version moved and whose events did not reach `core_outbox` yet is dirty.
//! `set_version` (the snapshot and pagination paths of the client) does the same WHEN THE VERSION
//! MOVES (an empty sync writes the state again at the version it already has, `bootstrapped` only:
//! no event zone follows, so a flag set there would never be cleared), and `delete_version` leaves
//! a dirty row. Clearing the flag is rule (b)'s (`crate::appstate`, W3a): at two quiet points, never
//! in the middle of a session, by [`KourierStore::clear_quiet`] (`DELETE FROM appstate_dirty WHERE
//! name = ? AND version <= ?`, [`clear_dirty`]), so a patch committed after the look keeps its flag.
//! The startup check "a dirty collection read when the store opens is a reason for a resync" landed in the SAME step
//! as the clearing (W6a1, `crate::appstate::startup`), never earlier: without the clearing every collection would
//! be dirty after its first patch and every start would ask the server for all of them again. Every write of a
//! version counts in
//! [`crate::appstate::Activity`] (inside its job), which is what the quiet look compares.

use std::collections::HashMap;

use async_trait::async_trait;
use kourier_db::rusqlite::types::Value;
use kourier_db::rusqlite::{Transaction, params, params_from_iter};
use wacore::appstate::hash::HashState;
use wacore::appstate::processor::AppStateMutationMAC;
use wacore::store::error::Result;
use wacore::store::traits::{AppStateSyncKey, AppSyncStore};

use super::{KourierStore, blob, exec, first, rows};
use crate::db::Points;
use crate::error::{invalid, sql, unreadable};
use crate::failpoints;

const PATCH: Points = Points {
    inside: failpoints::INSIDE_COMMIT_PATCH,
    after: failpoints::AFTER_COMMIT_PATCH,
};

const VERSION: Points = Points {
    inside: failpoints::INSIDE_SET_VERSION,
    after: failpoints::AFTER_SET_VERSION,
};

/// How many index MACs one `IN (…)` list holds (SQLite's variable limit is far above this).
const IN_LIST: usize = 500;

/// A `HashState` as stored: its JSON (version, the 128-byte hash, `index_value_map`,
/// `mac_mismatch_fatal`, `bootstrapped`), the shape upstream persists.
fn encode(state: &HashState) -> Result<Vec<u8>> {
    serde_json::to_vec(state).map_err(|_| unreadable("a hash state could not be serialized"))
}

fn decode(record: &[u8]) -> Result<HashState> {
    serde_json::from_slice(record).map_err(|e| {
        tracing::error!(
            line = e.line(),
            column = e.column(),
            "a stored hash state could not be read"
        );
        unreadable("a stored hash state could not be read")
    })
}

/// The version of the state stored for `name`, in the transaction that is about to replace it;
/// `None` when there is none, or when it cannot be read (the replacement then counts as a move).
fn stored_version(tx: &Transaction<'_>, op: &'static str, name: &str) -> Result<Option<u64>> {
    /// Only the version of a stored `HashState`: the rest of the record is skipped, not built.
    #[derive(serde::Deserialize)]
    struct Versioned {
        version: u64,
    }
    Ok(first(
        tx,
        op,
        "SELECT state FROM app_state_versions WHERE name = ?1",
        params![name],
        |row| blob(row, 0),
    )?
    .and_then(|record| serde_json::from_slice::<Versioned>(&record).ok())
    .map(|stored| stored.version))
}

/// Marks `name` dirty at `version`. The version of the row follows the version of the collection.
fn mark_dirty(tx: &Transaction<'_>, op: &'static str, name: &str, version: u64) -> Result<()> {
    let version =
        i64::try_from(version).map_err(|_| invalid("a collection version is out of range"))?;
    exec(
        tx,
        op,
        "INSERT INTO appstate_dirty(name, version) VALUES (?1, ?2) \
         ON CONFLICT(name) DO UPDATE SET version = excluded.version",
        params![name, version],
    )?;
    Ok(())
}

fn put_version(
    tx: &Transaction<'_>,
    op: &'static str,
    name: &str,
    state: &HashState,
) -> Result<()> {
    exec(
        tx,
        op,
        "INSERT INTO app_state_versions(name, state) VALUES (?1, ?2) \
         ON CONFLICT(name) DO UPDATE SET state = excluded.state",
        params![name, encode(state)?],
    )?;
    Ok(())
}

fn put_mac(
    tx: &Transaction<'_>,
    op: &'static str,
    name: &str,
    version: u64,
    mac: &AppStateMutationMAC,
) -> Result<()> {
    let version =
        i64::try_from(version).map_err(|_| invalid("a collection version is out of range"))?;
    exec(
        tx,
        op,
        "INSERT INTO app_state_mutation_macs(name, index_mac, version, value_mac) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(name, index_mac) DO UPDATE SET version = excluded.version, \
         value_mac = excluded.value_mac",
        params![name, mac.index_mac, version, mac.value_mac],
    )?;
    Ok(())
}

/// Clears the dirty flag of `name` for events read at `version`: a flag set by a later version stays.
/// Called by [`KourierStore::clear_quiet`] (rule (b)) and by nothing else: no event zone clears a flag (upstream gives
/// no "every event of this version was dispatched" to clear on), the quiet looks of `appstate.rs` do.
pub(crate) fn clear_dirty(tx: &Transaction<'_>, name: &str, version: u64) -> Result<usize> {
    let version =
        i64::try_from(version).map_err(|_| invalid("a collection version is out of range"))?;
    exec(
        tx,
        "clear appstate dirty failed",
        "DELETE FROM appstate_dirty WHERE name = ?1 AND version <= ?2",
        params![name, version],
    )
}

impl KourierStore {
    /// The collections whose version moved and whose events have not reached the core yet:
    /// `(name, version)` in name order.
    pub(crate) async fn dirty_collections(&self) -> Result<Vec<(String, u64)>> {
        const OP: &str = "read dirty collections failed";
        self.read(move |conn| {
            let found = rows(
                conn,
                OP,
                "SELECT name, version FROM appstate_dirty ORDER BY name",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?;
            found
                .into_iter()
                .map(|(name, version)| {
                    u64::try_from(version)
                        .map(|version| (name, version))
                        .map_err(|_| invalid("a stored collection version is out of range"))
                })
                .collect()
        })
        .await
    }
}

/// [`KourierStore::redirty`] inside the caller's zone: marks each of `names` that has a stored version above 0 dirty at
/// that version (a flag that stands stays as it is). The number of flags set. The write counter moves for each name
/// that had a version, like every other version write, so a quiet look in flight is void.
pub(crate) fn redirty_in(
    tx: &Transaction<'_>,
    activity: &crate::appstate::Activity,
    names: &[String],
) -> Result<usize> {
    const OP: &str = "mark grown collections dirty failed";
    let mut marked = 0;
    for name in names {
        let Some(version) = stored_version(tx, OP, name)?.filter(|version| *version > 0) else {
            continue;
        };
        let version =
            i64::try_from(version).map_err(|_| invalid("a collection version is out of range"))?;
        activity.wrote();
        marked += exec(
            tx,
            OP,
            "INSERT INTO appstate_dirty(name, version) VALUES (?1, ?2) \
             ON CONFLICT(name) DO NOTHING",
            params![name, version],
        )?;
    }
    Ok(marked)
}

impl KourierStore {
    /// A build whose app-state map covers more verbs (`appstate::startup::MAP_VERSION`, decision D4): marks each of
    /// `names` that has a stored version above 0 dirty at that version, so the start-up resync reads it again once.
    /// A collection with no stored version, or one at version 0 (bootstrapped, nothing ever applied), never moved, so
    /// no event of it was lost: it is left alone. A flag that stands stays as it is. The number of flags set.
    pub(crate) async fn redirty(&self, names: Vec<String>) -> Result<usize> {
        let activity = self.activity().clone();
        self.write("mark grown collections dirty failed", move |tx| {
            redirty_in(tx, &activity, &names)
        })
        .await
    }
}

impl KourierStore {
    /// Clearing point of rule (b) (`crate::appstate`): in ONE job, in the writer's own order, first
    /// check that no version was written since the caller's quiet look (`seen` = the counter then):
    /// `Ok(None)` and nothing cleared when one was. Then clear the flag of every collection (of
    /// `only`, when given) whose stored version is at or above the flag's, with the version read in
    /// this very transaction, so a concurrent sync cannot move it between the look and the clear. A
    /// collection with no stored version (`delete_version` left it dirty: it waits for a snapshot)
    /// keeps its flag. The number of flags cleared.
    pub(crate) async fn clear_quiet(
        &self,
        seen: u64,
        only: Option<Vec<String>>,
    ) -> Result<Option<usize>> {
        const OP: &str = "clear appstate dirty failed";
        let activity = self.activity().clone();
        self.write(OP, move |tx| {
            if activity.writes() != seen {
                return Ok(None);
            }
            let flags = rows(
                tx,
                OP,
                "SELECT name, version FROM appstate_dirty ORDER BY name",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?;
            let mut cleared = 0;
            for (name, flagged) in flags {
                if only.as_ref().is_some_and(|only| !only.contains(&name)) {
                    continue;
                }
                let Some(stored) = stored_version(tx, OP, &name)? else {
                    continue;
                };
                if u64::try_from(flagged).is_ok_and(|flagged| flagged <= stored) {
                    cleared += clear_dirty(tx, &name, stored)?;
                }
            }
            Ok(Some(cleared))
        })
        .await
    }
}

#[async_trait]
impl AppSyncStore for KourierStore {
    async fn get_sync_key(&self, key_id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        const OP: &str = "get sync key failed";
        let key_id = key_id.to_vec();
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT key_data, fingerprint, timestamp FROM app_state_keys WHERE key_id = ?1",
                params![key_id],
                |row| {
                    Ok(AppStateSyncKey {
                        key_data: blob(row, 0)?,
                        fingerprint: blob(row, 1)?,
                        timestamp: row.get(2)?,
                    })
                },
            )
        })
        .await
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> Result<()> {
        const OP: &str = "set sync key failed";
        let key_id = key_id.to_vec();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "INSERT INTO app_state_keys(key_id, key_data, fingerprint, timestamp) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(key_id) DO UPDATE SET key_data = excluded.key_data, \
                 fingerprint = excluded.fingerprint, timestamp = excluded.timestamp",
                params![key_id, key.key_data, key.fingerprint, key.timestamp],
            )?;
            Ok(())
        })
        .await
    }

    /// `None` = never synced. A collection that synced and is empty has a record at version 0.
    async fn get_version(&self, name: &str) -> Result<Option<HashState>> {
        const OP: &str = "get version failed";
        let name = name.to_owned();
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT state FROM app_state_versions WHERE name = ?1",
                params![name],
                |row| blob(row, 0),
            )?
            .map(|record| decode(&record))
            .transpose()
        })
        .await
    }

    /// Back to the never-synced state; the collection is dirty until a snapshot rebuilds it.
    async fn delete_version(&self, name: &str) -> Result<()> {
        const OP: &str = "delete version failed";
        let name = name.to_owned();
        let activity = self.activity().clone();
        self.write_at(OP, VERSION, move |tx| {
            activity.wrote();
            exec(
                tx,
                OP,
                "DELETE FROM app_state_versions WHERE name = ?1",
                params![name],
            )?;
            // A dirty row stays where it is; a clean collection becomes dirty at version 0.
            exec(
                tx,
                OP,
                "INSERT INTO appstate_dirty(name, version) VALUES (?1, 0) \
                 ON CONFLICT(name) DO NOTHING",
                params![name],
            )?;
            Ok(())
        })
        .await
    }

    /// The snapshot and pagination paths: the version and, when the version MOVES, its dirty flag,
    /// one transaction. A write at the version already stored (upstream's empty sync marks the
    /// collection `bootstrapped` and nothing else) changes the state and leaves the flag as it is: a
    /// clean collection stays clean, a dirty one stays dirty.
    async fn set_version(&self, name: &str, state: HashState) -> Result<()> {
        const OP: &str = "set version failed";
        let name = name.to_owned();
        let activity = self.activity().clone();
        self.write_at(OP, VERSION, move |tx| {
            activity.wrote();
            let moved = stored_version(tx, OP, &name)? != Some(state.version);
            put_version(tx, OP, &name, &state)?;
            if moved {
                mark_dirty(tx, OP, &name, state.version)?;
            }
            Ok(())
        })
        .await
    }

    async fn put_mutation_macs(
        &self,
        name: &str,
        version: u64,
        mutations: &[AppStateMutationMAC],
    ) -> Result<()> {
        const OP: &str = "put mutation macs failed";
        let name = name.to_owned();
        let mutations = mutations.to_vec();
        self.write(OP, move |tx| {
            for mac in &mutations {
                put_mac(tx, OP, &name, version, mac)?;
            }
            Ok(())
        })
        .await
    }

    async fn get_mutation_mac(&self, name: &str, index_mac: &[u8]) -> Result<Option<Vec<u8>>> {
        const OP: &str = "get mutation mac failed";
        let (name, index_mac) = (name.to_owned(), index_mac.to_vec());
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT value_mac FROM app_state_mutation_macs WHERE name = ?1 AND index_mac = ?2",
                params![name, index_mac],
                |row| blob(row, 0),
            )
        })
        .await
    }

    /// One query per 500 index MACs (`IN (…)`), not one per MAC. The text of the query depends on
    /// the number of MACs, so it is prepared and dropped (`prepare`), never put into the connection's
    /// statement cache: a sync with many patch sizes would evict the crate's fixed statements.
    async fn get_mutation_macs(
        &self,
        name: &str,
        index_macs: &[[u8; 32]],
    ) -> Result<HashMap<[u8; 32], Vec<u8>>> {
        const OP: &str = "get mutation macs failed";
        let name = name.to_owned();
        let index_macs = index_macs.to_vec();
        self.read(move |conn| {
            let mut found = HashMap::with_capacity(index_macs.len());
            for chunk in index_macs.chunks(IN_LIST) {
                let marks = vec!["?"; chunk.len()].join(",");
                let statement = format!(
                    "SELECT index_mac, value_mac FROM app_state_mutation_macs \
                     WHERE name = ?1 AND index_mac IN ({marks})"
                );
                let mut values: Vec<Value> = vec![name.clone().into()];
                values.extend(chunk.iter().map(|mac| Value::from(mac.to_vec())));
                let answers = conn
                    .prepare(&statement)
                    .and_then(|mut stmt| {
                        stmt.query_map(params_from_iter(values), |row| {
                            Ok((blob(row, 0)?, blob(row, 1)?))
                        })
                        .and_then(|found| found.collect::<std::result::Result<Vec<_>, _>>())
                    })
                    .map_err(sql(OP))?;
                for (index_mac, value_mac) in answers {
                    let index_mac = <[u8; 32]>::try_from(index_mac.as_slice())
                        .map_err(|_| invalid("a stored index mac is not 32 bytes"))?;
                    found.insert(index_mac, value_mac);
                }
            }
            Ok(found)
        })
        .await
    }

    async fn delete_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> Result<()> {
        const OP: &str = "delete mutation macs failed";
        let name = name.to_owned();
        let index_macs = index_macs.to_vec();
        self.write(OP, move |tx| {
            for index_mac in &index_macs {
                exec(
                    tx,
                    OP,
                    "DELETE FROM app_state_mutation_macs WHERE name = ?1 AND index_mac = ?2",
                    params![name, index_mac],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// The required atomic unit. Removals first, then additions (an index in both ends with the
    /// added value), then the version and its dirty flag; all in one transaction, so storage holds
    /// the previous state or the complete patch, never a new version with some of its MACs. `Ok`
    /// only after `COMMIT`.
    async fn commit_patch(
        &self,
        name: &str,
        state: HashState,
        removed_index_macs: &[Vec<u8>],
        added: &[AppStateMutationMAC],
    ) -> Result<()> {
        const OP: &str = "commit patch failed";
        let name = name.to_owned();
        let removed = removed_index_macs.to_vec();
        let added = added.to_vec();
        let activity = self.activity().clone();
        self.write_at(OP, PATCH, move |tx| {
            activity.wrote();
            for index_mac in &removed {
                exec(
                    tx,
                    OP,
                    "DELETE FROM app_state_mutation_macs WHERE name = ?1 AND index_mac = ?2",
                    params![name, index_mac],
                )?;
            }
            for mac in &added {
                put_mac(tx, OP, &name, state.version, mac)?;
            }
            put_version(tx, OP, &name, &state)?;
            mark_dirty(tx, OP, &name, state.version)
        })
        .await
    }

    async fn clear_mutation_macs(&self, name: &str) -> Result<()> {
        const OP: &str = "clear mutation macs failed";
        let name = name.to_owned();
        self.write(OP, move |tx| {
            exec(
                tx,
                OP,
                "DELETE FROM app_state_mutation_macs WHERE name = ?1",
                params![name],
            )?;
            Ok(())
        })
        .await
    }

    /// The key with the newest OWN time (`timestamp`), ties by key id: a phone that shares an older
    /// key again after a newer one does not make the old one the latest (the in-memory store takes
    /// the last call, upstream's SQLite the highest key id, whatsmeow the key's time).
    async fn get_latest_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        const OP: &str = "get latest sync key id failed";
        self.read(move |conn| {
            first(
                conn,
                OP,
                "SELECT key_id FROM app_state_keys ORDER BY timestamp DESC, key_id DESC LIMIT 1",
                [],
                |row| blob(row, 0),
            )
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_db::DbKey;

    use crate::store::StoreOwner;

    fn owner() -> (tempfile::TempDir, StoreOwner) {
        let dir = tempfile::tempdir().expect("temp dir");
        let owner = StoreOwner::open(
            &dir.path().join("backend.db"),
            &DbKey::from([5; 32]),
            "wa-test",
        )
        .expect("open");
        (dir, owner)
    }

    /// The clearing statement rule (b) uses: a flag set by a LATER version than the events were read at
    /// stays, one at or below it goes.
    #[tokio::test]
    async fn clearing_keeps_a_flag_set_after_the_events_were_read() {
        let (_dir, owner) = owner();
        let s = owner.store();
        let state = |version| HashState {
            version,
            ..HashState::default()
        };
        s.set_version("regular", state(4)).await.unwrap();
        s.set_version("critical", state(7)).await.unwrap();
        assert_eq!(
            s.dirty_collections().await.unwrap(),
            [("critical".to_owned(), 7), ("regular".to_owned(), 4)]
        );
        // Events were read at version 4; a patch commits version 5 before the clearing zone runs.
        s.commit_patch("regular", state(5), &[], &[]).await.unwrap();
        let cleared = s
            .write("clear", |tx| {
                let a = clear_dirty(tx, "regular", 4)?; // 5 > 4: stays
                let b = clear_dirty(tx, "critical", 7)?; // 7 <= 7: goes
                let c = clear_dirty(tx, "absent", 9)?; // nothing to clear
                Ok((a, b, c))
            })
            .await
            .unwrap();
        assert_eq!(cleared, (0, 1, 0));
        assert_eq!(
            s.dirty_collections().await.unwrap(),
            [("regular".to_owned(), 5)]
        );
        // A later zone that read version 5 clears it.
        let n = s
            .write("clear", |tx| clear_dirty(tx, "regular", 5))
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert!(s.dirty_collections().await.unwrap().is_empty());
        // Version numbers that do not fit are refused, never wrapped.
        let err = s
            .write("clear", |tx| clear_dirty(tx, "regular", u64::MAX))
            .await
            .expect_err("out of range");
        assert!(err.to_string().contains("data validation failed"), "{err}");
        owner.close();
    }

    /// `get_mutation_macs` builds a statement text per chunk length. Those texts must not go through
    /// the connection's statement cache (128 entries): a sync with many patch sizes would push the
    /// crate's fixed statements out of it, and each would be prepared again on its next use.
    #[tokio::test]
    async fn macs_of_many_sizes_do_not_push_the_fixed_statements_out_of_the_cache() {
        use kourier_db::rusqlite::StatementStatus;
        // The text of `get_mutation_mac`, a fixed statement of this module.
        const FIXED: &str =
            "SELECT value_mac FROM app_state_mutation_macs WHERE name = ?1 AND index_mac = ?2";
        let (_dir, owner) = owner();
        let s = owner.store();
        s.get_mutation_mac("regular", &[0; 32]).await.unwrap();
        // More distinct chunk lengths than the cache holds.
        for n in 1..=140u8 {
            let macs = vec![[n; 32]; usize::from(n)];
            s.get_mutation_macs("regular", &macs).await.unwrap();
        }
        let runs = s
            .read(|conn| {
                let statement = conn.prepare_cached(FIXED).map_err(sql("probe"))?;
                Ok(statement.get_status(StatementStatus::Run))
            })
            .await
            .unwrap();
        assert!(
            runs >= 1,
            "the fixed statement was pushed out of the cache and prepared again ({runs} runs)"
        );
        owner.close();
    }
}
