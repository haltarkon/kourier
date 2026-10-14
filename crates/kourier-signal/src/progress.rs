//! How far the work of a newly linked account is that the user waits for while the chats are
//! already usable (link feedback L3, `BackendEvent::AccountProgress`): the delivery of an
//! imported history (messages delivered and in all), then the profiles and the groups the link
//! still fetches. Memory only: the flag [`Settling`] is set at a fresh link's start, when a
//! history transfer runs, and at an actor start that finds groups still loading; while it is
//! set the job runner reports after every batch and when idle (`connection::finish_link`), and
//! the delivery after every chunk of messages, each time only when the numbers changed. When
//! nothing is left (no transfer holding receiving back, no login owing its `Done`, no
//! sync of the link waiting, no profile fetch and no refresh of a group the server has not
//! given yet) it reports `finished` once and clears the flag.

use std::cell::Cell;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use kourier_model::AccountProgress;

use crate::actor::Shared;
use crate::error::Result;
use crate::jobs::JobKind;
use crate::net::SignalTransport;

/// Failed attempts after which a sync of the link no longer counts as work left (the same
/// bound as the link's own wait, `connection::LINK_SYNC_ATTEMPTS`).
const SYNC_ATTEMPTS: i64 = 3;

/// The memory of what was reported.
#[derive(Debug, Default)]
pub(crate) struct Settling {
    /// Reports are owed: work the user waits for may be running.
    on: Cell<bool>,
    /// Messages of the imported history delivered so far and in all (`0, 0` = none).
    messages: Cell<(u64, u64)>,
    /// The last report, so an unchanged one is not sent again.
    last: Cell<Option<AccountProgress>>,
}

impl Settling {
    /// Work the user waits for begins (a fresh link, a transfer, groups still loading).
    pub(crate) fn begin(&self) {
        if !self.on.replace(true) {
            tracing::debug!("account progress is reported");
        }
    }

    /// Whether reports are owed.
    pub(crate) fn is_on(&self) -> bool {
        self.on.get()
    }

    /// The delivery of the imported history stands at `done` of `total` messages.
    pub(crate) fn set_messages(&self, done: u64, total: u64) {
        self.messages.set((done.min(total), total));
    }

    fn end(&self) {
        self.on.set(false);
        self.messages.set((0, 0));
        self.last.set(None);
    }
}

/// What is left in `backend.db`: profile fetches, refreshes of groups the server has not given
/// yet, and whether a sync of the link still runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Left {
    pub(crate) profiles: u32,
    pub(crate) groups: u32,
    pub(crate) syncing: bool,
}

/// Reads [`Left`].
pub(crate) fn left(conn: &Connection) -> Result<Left> {
    let profiles: i64 = conn
        .prepare_cached("SELECT count(*) FROM jobs WHERE kind = ?1")?
        .query_row([JobKind::ProfileFetch.as_str()], |r| r.get(0))?;
    // A group counts while its refresh is queued and the server has not given its state: it
    // says "still loading" and cannot be sent to. A refresh that is dropped (a permanent error)
    // ends the count, so the report always comes to its end.
    let groups: i64 = conn
        .prepare_cached(
            "SELECT count(*) FROM jobs j JOIN groups g ON j.key = lower(hex(g.group_id))
              WHERE j.kind = ?1 AND g.state IS NULL AND g.membership NOT IN ('left', 'gone')",
        )?
        .query_row([JobKind::GroupRefresh.as_str()], |r| r.get(0))?;
    let syncing: i64 = conn
        .prepare_cached("SELECT count(*) FROM jobs WHERE kind IN (?1, ?2, ?3) AND attempts < ?4")?
        .query_row(
            (
                JobKind::SyncRequest.as_str(),
                JobKind::ContactSync.as_str(),
                JobKind::StorageSync.as_str(),
                SYNC_ATTEMPTS,
            ),
            |r| r.get(0),
        )?;
    Ok(Left {
        profiles: u32::try_from(profiles).unwrap_or(u32::MAX),
        groups: u32::try_from(groups).unwrap_or(u32::MAX),
        syncing: syncing > 0,
    })
}

/// How long after the link a start still takes groups that wait for their state for the tail
/// of that link. A group that stays without a state for longer (the server refuses it, or
/// the account is not a member any more) is not news at the start of an account the user has
/// used for days: no banner and no question on quitting for it.
pub(crate) const TAIL_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Whether an account that starts has groups still loading (a link's tail cut off by a quit):
/// groups the core has (`features_hash`) whose state the server has not given, not left or gone
/// (the same groups `maintenance::enqueue_stranded_group_refreshes` queues a refresh for),
/// and only within [`TAIL_WINDOW_MS`] of the link (`account.linked_at`, milliseconds; a clock
/// that is behind the link counts as within).
pub(crate) fn groups_loading(conn: &Connection, now_ms: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM groups WHERE state IS NULL AND features_hash IS NOT NULL
            AND membership NOT IN ('left', 'gone'))
          AND EXISTS (SELECT 1 FROM account WHERE linked_at > ?1 - ?2)",
        (now_ms, TAIL_WINDOW_MS),
        |r| r.get(0),
    )?)
}

/// The report for `left` with `messages` delivered of the import, or `None` when nothing is
/// left (`busy`: a transfer holds receiving back or a login owes its `Done`).
pub(crate) fn report_for(messages: (u64, u64), left: Left, busy: bool) -> Option<AccountProgress> {
    if !busy && !left.syncing && left.profiles == 0 && left.groups == 0 {
        return None;
    }
    Some(AccountProgress {
        messages_done: messages.0,
        messages_total: messages.1,
        profiles_left: left.profiles,
        groups_left: left.groups,
        finished: false,
    })
}

/// Reports how far the work is, when reports are owed and the numbers changed; reports
/// `finished` once and stops when nothing is left. Fails only when `backend.db` fails or the
/// core is gone.
pub(crate) async fn report<T: SignalTransport>(shared: &Shared<T>) -> Result<()> {
    let settling = &shared.settling;
    if !settling.is_on() {
        return Ok(());
    }
    let holds = shared.transfer.holds();
    if !holds {
        // The import is delivered (or there was none): its count is no longer news.
        settling.set_messages(0, 0);
    }
    let left = shared.db.read(left)?;
    let busy = holds || shared.login_pending.get();
    match report_for(settling.messages.get(), left, busy) {
        Some(progress) => {
            if settling.last.get() != Some(progress) {
                settling.last.set(Some(progress));
                shared.emit(BackendEvent::AccountProgress(progress)).await?;
            }
        }
        None => {
            settling.end();
            tracing::info!("the link's work is done");
            shared
                .emit(BackendEvent::AccountProgress(AccountProgress::finished()))
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The link's time in the tests that read `groups_loading`.
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn nothing_left_is_the_end_and_anything_left_is_a_report() {
        let none = Left::default();
        assert_eq!(report_for((0, 0), none, false), None);
        assert!(
            report_for((0, 0), none, true).is_some(),
            "a transfer or login is work"
        );
        let syncing = Left {
            syncing: true,
            ..none
        };
        assert!(report_for((0, 0), syncing, false).is_some());
        let some = Left {
            profiles: 120,
            groups: 8,
            syncing: false,
        };
        let p = report_for((12_400, 50_000), some, false).unwrap();
        assert_eq!(
            (
                p.messages_done,
                p.messages_total,
                p.profiles_left,
                p.groups_left
            ),
            (12_400, 50_000, 120, 8)
        );
        assert!(!p.finished);
    }

    /// The counts read from `backend.db`: a group counts only while its refresh is queued
    /// and the server has not given its state (not left or gone); a sync of the link counts
    /// until it failed three times.
    #[test]
    fn what_is_left_is_read_from_the_jobs_and_the_groups() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([4; 32]),
        )
        .unwrap();
        db.zone(|tx| -> Result<()> {
            for (i, (state, membership)) in [
                (None, "unknown"),
                (None, "pending"),
                (Some(vec![1u8]), "member"),
                (None, "left"),
                (None, "unknown"),
            ]
            .into_iter()
            .enumerate()
            {
                let id = vec![0xA0 + u8::try_from(i).unwrap(); 32];
                tx.execute(
                    "INSERT INTO groups(master_key, group_id, updated_at, state, membership,
                        features_hash) VALUES (?1, ?2, 0, ?3, ?4, 1)",
                    (vec![u8::try_from(i).unwrap(); 32], &id, state, membership),
                )?;
                // The last group has no refresh queued.
                if i < 4 {
                    crate::jobs::enqueue(
                        tx,
                        JobKind::GroupRefresh,
                        &crate::groups::job_key(&id),
                        0,
                        None,
                    )?;
                }
            }
            for person in ["a", "b", "c"] {
                crate::jobs::enqueue(tx, JobKind::ProfileFetch, person, 0, None)?;
            }
            crate::jobs::enqueue(tx, JobKind::StorageSync, "", 0, None)?;
            Ok(())
        })
        .unwrap();
        let read = db.read(left).unwrap();
        assert_eq!(
            read,
            Left {
                profiles: 3,
                groups: 2,
                syncing: true,
            }
        );
        // The account was linked just now: its stranded groups are the link's tail.
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO account(id, aci, pni, e164, device_id, password,
                   aci_registration_id, pni_registration_id, profile_key, master_key,
                   storage_key, linked_at)
                 VALUES (1, 'a', 'p', '+15550100', 2, 'pw', 1, 2, x'00', x'00', x'00', ?1)",
                [NOW],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(db.read(|c| groups_loading(c, NOW)).unwrap());
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "UPDATE jobs SET attempts = 3 WHERE kind = 'storage_sync'",
                [],
            )?;
            tx.execute("UPDATE groups SET state = x'01'", [])?;
            Ok(())
        })
        .unwrap();
        let read = db.read(left).unwrap();
        assert_eq!((read.groups, read.syncing), (0, false));
        assert!(!db.read(|c| groups_loading(c, NOW)).unwrap());
    }

    /// Link feedback L3 (F3, F5): a start counts a group that waits for its state as the
    /// link's tail only within a day of the link. An older account's stranded group gives no
    /// banner and no question on quitting; a clock behind the link counts as within.
    #[test]
    fn a_stranded_group_is_the_links_tail_only_within_a_day_of_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([5; 32]),
        )
        .unwrap();
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO account(id, aci, pni, e164, device_id, password,
                   aci_registration_id, pni_registration_id, profile_key, master_key,
                   storage_key, linked_at)
                 VALUES (1, 'a', 'p', '+15550100', 2, 'pw', 1, 2, x'00', x'00', x'00', ?1)",
                [NOW],
            )?;
            tx.execute(
                "INSERT INTO groups(master_key, group_id, updated_at, state, membership,
                    features_hash) VALUES (x'01', x'02', 0, NULL, 'unknown', 1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let at = |now: i64| db.read(|c| groups_loading(c, now)).unwrap();
        assert!(at(NOW), "just linked");
        assert!(
            at(NOW + TAIL_WINDOW_MS - 1),
            "a moment before the day is over"
        );
        assert!(!at(NOW + TAIL_WINDOW_MS), "a day after the link");
        assert!(!at(NOW + 30 * TAIL_WINDOW_MS), "a month after the link");
        assert!(at(NOW - 3_600_000), "a clock that is behind the link");
        db.zone(|tx| -> Result<()> {
            tx.execute("UPDATE groups SET state = x'01'", [])?;
            Ok(())
        })
        .unwrap();
        assert!(!at(NOW), "the group has its state");
    }

    #[test]
    fn the_delivered_count_never_passes_the_total() {
        let s = Settling::default();
        s.set_messages(60, 50);
        assert_eq!(s.messages.get(), (50, 50));
        s.begin();
        assert!(s.is_on());
        s.end();
        assert!(!s.is_on());
        assert_eq!(s.messages.get(), (0, 0));
    }
}
