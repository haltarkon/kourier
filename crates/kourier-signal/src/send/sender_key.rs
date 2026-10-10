//! Our sender-key chain per group (§6.7 step 5; ports the `senderKeyInfo` handling of
//! `SD/ts/util/sendToGroup.preload.ts`: `saveSenderKeyInfo`, `resetSenderKey`,
//! `getSenderKeyExpireDuration`, `_analyzeSenderKeyDevices`).
//!
//! `sender_key_info` keeps the distribution id of our chain in each group and when the chain
//! started; `sender_key_devices` the member devices that have our distribution message for it
//! (device id and registration id). A chain older than `desktop.senderKeyMaxAge` (at most 90 days)
//! or one a removed member knows is reset: our chain is deleted, the device list cleared, the
//! distribution id kept, so every member device gets a new distribution message before the next
//! sender-key message. The chain itself lives in `sender_keys` under our ACI and device id, like
//! the chains of other members.

use std::collections::BTreeSet;
use std::time::Duration;

use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_protocol::create_sender_key_distribution_message;
use uuid::Uuid;

use crate::db::Account;
use crate::error::{Error, Result};
use crate::maintenance::load_remote_config;
use crate::store::{IdentityKind, SqlSenderKeyStore, ready};

/// The remote-config entry of the longest chain age, in milliseconds.
pub(crate) const MAX_AGE_KEY: &str = "desktop.senderKeyMaxAge";
/// The longest chain age Desktop accepts, and its default (`MAX_SENDER_KEY_EXPIRE_DURATION`).
pub(crate) const MAX_AGE_CAP: Duration = Duration::from_secs(90 * 24 * 3600);

/// Our chain in one group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chain {
    /// The distribution id (a uuid; the same across resets).
    pub(crate) distribution_id: Uuid,
    /// When the chain started (ms).
    pub(crate) created_at: i64,
}

/// A member device that has our distribution message.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MemberDevice {
    /// The member (service id string).
    pub(crate) recipient: String,
    /// The device id.
    pub(crate) device: u32,
    /// Its registration id when it got the message (a re-registered device has a new one).
    pub(crate) registration_id: u32,
}

/// Our chain in group `group_id`, if one was started.
pub(crate) fn chain(conn: &Connection, group_id: &[u8]) -> Result<Option<Chain>> {
    let row: Option<(Vec<u8>, i64)> = conn
        .prepare_cached(
            "SELECT distribution_id, created_at FROM sender_key_info WHERE group_id = ?1",
        )?
        .query_row([group_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    row.map(|(id, created_at)| {
        Ok(Chain {
            distribution_id: Uuid::from_slice(&id)
                .map_err(|_| Error::Decode("sender key distribution id"))?,
            created_at,
        })
    })
    .transpose()
}

/// Whether we have a chain in group `group_id`.
pub(crate) fn has_chain(conn: &Connection, group_id: &[u8]) -> Result<bool> {
    Ok(chain(conn, group_id)?.is_some())
}

/// Our chain in `group_id`, started (a new distribution id) when there is none.
pub(crate) fn ensure(conn: &Connection, group_id: &[u8], now: i64) -> Result<Chain> {
    if let Some(chain) = chain(conn, group_id)? {
        return Ok(chain);
    }
    let chain = Chain {
        distribution_id: Uuid::new_v4(),
        created_at: now,
    };
    conn.prepare_cached(
        "INSERT INTO sender_key_info(group_id, distribution_id, created_at) VALUES (?1, ?2, ?3)",
    )?
    .execute((group_id, &chain.distribution_id.as_bytes()[..], now))?;
    Ok(chain)
}

/// Starts a new chain under the same distribution id (`resetSenderKey`): our chain is deleted
/// and no member device has the new one yet.
pub(crate) fn reset(conn: &Connection, account: &Account, group_id: &[u8], now: i64) -> Result<()> {
    let Some(chain) = chain(conn, group_id)? else {
        return Ok(());
    };
    conn.prepare_cached(
        "DELETE FROM sender_keys WHERE sender = ?1 AND device = ?2 AND distribution_id = ?3",
    )?
    .execute((
        libsignal_core::ServiceId::from(account.aci).service_id_string(),
        u32::from(account.device_id),
        &chain.distribution_id.as_bytes()[..],
    ))?;
    conn.prepare_cached("DELETE FROM sender_key_devices WHERE group_id = ?1")?
        .execute([group_id])?;
    conn.prepare_cached("UPDATE sender_key_info SET created_at = ?2 WHERE group_id = ?1")?
        .execute((group_id, now))?;
    Ok(())
}

/// How old a chain may get (`desktop.senderKeyMaxAge` in ms, at most [`MAX_AGE_CAP`], which is
/// also the default).
pub(crate) fn max_age(conn: &Connection) -> Result<Duration> {
    let config = load_remote_config(conn)?;
    Ok(config
        .get(MAX_AGE_KEY)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(MAX_AGE_CAP, |ms| Duration::from_millis(ms).min(MAX_AGE_CAP)))
}

/// Whether `chain` is older than `max_age` at `now`.
pub(crate) fn expired(chain: &Chain, max_age: Duration, now: i64) -> bool {
    let max = i64::try_from(max_age.as_millis()).unwrap_or(i64::MAX);
    now.saturating_sub(chain.created_at) > max
}

/// The member devices that have our distribution message for the current chain.
pub(crate) fn devices(conn: &Connection, group_id: &[u8]) -> Result<BTreeSet<MemberDevice>> {
    Ok(conn
        .prepare_cached(
            "SELECT recipient, device, registration_id FROM sender_key_devices
             WHERE group_id = ?1",
        )?
        .query_map([group_id], |r| {
            Ok(MemberDevice {
                recipient: r.get(0)?,
                device: r.get(1)?,
                registration_id: r.get(2)?,
            })
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?)
}

/// Records that `devices` got our distribution message (a device seen with a new registration
/// id replaces its old entry).
pub(crate) fn add_devices<'d>(
    conn: &Connection,
    group_id: &[u8],
    devices: impl IntoIterator<Item = &'d MemberDevice>,
) -> Result<()> {
    for device in devices {
        conn.prepare_cached(
            "INSERT INTO sender_key_devices(group_id, recipient, device, registration_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(group_id, recipient, device)
             DO UPDATE SET registration_id = excluded.registration_id",
        )?
        .execute((
            group_id,
            &device.recipient,
            device.device,
            device.registration_id,
        ))?;
    }
    Ok(())
}

/// Forgets `devices` (gone, stale, or replaced).
pub(crate) fn remove_devices<'d>(
    conn: &Connection,
    group_id: &[u8],
    devices: impl IntoIterator<Item = &'d MemberDevice>,
) -> Result<()> {
    for device in devices {
        conn.prepare_cached(
            "DELETE FROM sender_key_devices WHERE group_id = ?1 AND recipient = ?2 AND device = ?3",
        )?
        .execute((group_id, &device.recipient, device.device))?;
    }
    Ok(())
}

/// Forgets device `device` of `recipient` in every group (it went stale: re-registered or
/// re-linked, so it needs a new distribution message).
pub(crate) fn forget_device(conn: &Connection, recipient: &str, device: u32) -> Result<()> {
    conn.prepare_cached("DELETE FROM sender_key_devices WHERE recipient = ?1 AND device = ?2")?
        .execute((recipient, device))?;
    Ok(())
}

/// How the member devices of a send compare with the devices that have our chain
/// (`_analyzeSenderKeyDevices`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DeviceDiff {
    /// Devices that need our distribution message (new, or re-registered).
    pub(crate) new: Vec<MemberDevice>,
    /// Devices that had it and are not in this send.
    pub(crate) removed: Vec<MemberDevice>,
}

/// Compares the devices of a send with `known`. A partial send (a resend to some members)
/// reports no removals: the members it leaves out did not leave.
pub(crate) fn diff(
    known: &BTreeSet<MemberDevice>,
    current: &BTreeSet<MemberDevice>,
    partial: bool,
) -> DeviceDiff {
    let new = current.difference(known).cloned().collect();
    let removed = if partial {
        Vec::new()
    } else {
        known.difference(current).cloned().collect()
    };
    DeviceDiff { new, removed }
}

/// Our `SenderKeyDistributionMessage` for `chain` (serialized), starting the chain in
/// `sender_keys` when it does not exist yet. Runs in the caller's zone.
pub(crate) fn distribution_message(
    conn: &Connection,
    account: &Account,
    chain: &Chain,
    now: i64,
) -> Result<Vec<u8>> {
    let mut store = SqlSenderKeyStore::new(conn, now);
    let message = ready(create_sender_key_distribution_message(
        &account.address(IdentityKind::Aci),
        chain.distribution_id,
        &mut store,
        &mut rand::rng(),
    ))??;
    Ok(message.serialized().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(recipient: &str, device: u32, registration_id: u32) -> MemberDevice {
        MemberDevice {
            recipient: recipient.to_owned(),
            device,
            registration_id,
        }
    }

    #[test]
    fn chains_start_reset_and_age_out() {
        let fixture = crate::receive::tests::fixture();
        let account = fixture.account.clone();
        let group = [7u8; 32];
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert!(!has_chain(tx, &group)?);
                let chain = ensure(tx, &group, 1_000)?;
                assert_eq!(ensure(tx, &group, 5_000)?, chain, "stable once started");
                let first = distribution_message(tx, &account, &chain, 1_000)?;
                assert_eq!(
                    distribution_message(tx, &account, &chain, 1_000)?,
                    first,
                    "the same chain until it is reset"
                );
                add_devices(tx, &group, &[device("a", 1, 11), device("b", 2, 22)])?;
                assert_eq!(devices(tx, &group)?.len(), 2);
                reset(tx, &account, &group, 9_000)?;
                let after = chain_or_panic(tx, &group);
                assert_eq!(after.distribution_id, chain.distribution_id, "id kept");
                assert_eq!(after.created_at, 9_000);
                assert!(devices(tx, &group)?.is_empty(), "nobody has the new chain");
                assert_ne!(
                    distribution_message(tx, &account, &after, 9_000)?,
                    first,
                    "a new chain"
                );
                assert_eq!(max_age(tx)?, MAX_AGE_CAP, "default");
                tx.execute(
                    "INSERT INTO remote_config(name, value) VALUES (?1, '3600000')",
                    [MAX_AGE_KEY],
                )?;
                assert_eq!(max_age(tx)?, Duration::from_secs(3600));
                assert!(!expired(
                    &after,
                    Duration::from_secs(3600),
                    9_000 + 3_600_000
                ));
                assert!(expired(
                    &after,
                    Duration::from_secs(3600),
                    9_001 + 3_600_000
                ));
                tx.execute(
                    "UPDATE remote_config SET value = '99999999999999' WHERE name = ?1",
                    [MAX_AGE_KEY],
                )?;
                assert_eq!(max_age(tx)?, MAX_AGE_CAP, "capped at 90 days");
                Ok(())
            })
            .unwrap();
    }

    fn chain_or_panic(conn: &Connection, group: &[u8]) -> Chain {
        chain(conn, group).unwrap().unwrap()
    }

    #[test]
    fn device_diffs_follow_desktop() {
        let known: BTreeSet<_> = [device("a", 1, 11), device("b", 1, 21), device("c", 1, 31)]
            .into_iter()
            .collect();
        // b re-registered (new registration id), c is not in this send, d is new.
        let current: BTreeSet<_> = [device("a", 1, 11), device("b", 1, 22), device("d", 3, 41)]
            .into_iter()
            .collect();
        let full = diff(&known, &current, false);
        assert_eq!(full.new, vec![device("b", 1, 22), device("d", 3, 41)]);
        assert_eq!(full.removed, vec![device("b", 1, 21), device("c", 1, 31)]);
        let partial = diff(&known, &current, true);
        assert_eq!(partial.new, full.new);
        assert!(partial.removed.is_empty(), "a resend reports no removals");
    }
}
