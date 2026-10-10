//! What the backend knows about other users (`peers`, §5.5): profile key and access key,
//! sealed-sender state, profile sharing (an accepted 1:1 chat), "share my number", the 1:1 timer
//! version and whether the server said they are unregistered.
//!
//! Desktop keeps the same facts on its conversation model (`SD/ts/models/conversations.preload.ts`
//! `setProfileKey`, `deriveAccessKeyIfNeeded`, `enableProfileSharing`, `shareMyPhoneNumber`) and
//! `SD/ts/types/SealedSender.std.ts`.

use kourier_db::rusqlite::{Connection, OptionalExtension};
use libsignal_core::{Aci, ServiceId};
use signal_crypto::Aes256Ctr32;
use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// Length of an unidentified-access key.
pub(crate) const ACCESS_KEY_LEN: usize = 16;

/// The access key of an "unrestricted" recipient (anyone may send sealed to them).
pub(crate) const ZERO_ACCESS_KEY: [u8; ACCESS_KEY_LEN] = [0; ACCESS_KEY_LEN];

/// `SEALED_SENDER` of Desktop: whether sealed sends to a user work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SealedSenderState {
    /// Never tried (or the profile key changed since).
    Unknown,
    /// A sealed send with their access key worked.
    Enabled,
    /// The server refused a sealed send (401/403): send unsealed.
    Disabled,
    /// Anyone may send sealed to them (zero access key).
    Unrestricted,
}

impl SealedSenderState {
    /// The `peers.sealed_sender` value.
    pub(crate) const fn as_i64(self) -> i64 {
        match self {
            Self::Unknown => 0,
            Self::Enabled => 1,
            Self::Disabled => 2,
            Self::Unrestricted => 3,
        }
    }

    fn from_i64(value: i64) -> Self {
        match value {
            1 => Self::Enabled,
            2 => Self::Disabled,
            3 => Self::Unrestricted,
            _ => Self::Unknown,
        }
    }
}

/// One `peers` row (the defaults when there is none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Peer {
    /// Their unidentified-access key, from their profile key.
    pub(crate) access_key: Option<[u8; ACCESS_KEY_LEN]>,
    /// Sealed-sender state.
    pub(crate) sealed: SealedSenderState,
    /// The chat is accepted: we share our profile with them.
    pub(crate) profile_sharing: bool,
    /// They know our number (they wrote to our PNI): sends carry a PNI signature.
    pub(crate) share_my_number: bool,
    /// The newest 1:1 timer version seen.
    pub(crate) expire_timer_version: u32,
    /// The 1:1 chat's disappearing-message timer in seconds, 0 = off.
    pub(crate) expire_timer: u32,
    /// The server said they are not registered.
    pub(crate) unregistered: bool,
    /// Hash of the conversation features last sent to the core (`Some`: the core has the chat).
    pub(crate) features_hash: Option<i64>,
    /// Blocked on our phone (Storage Service, blocked sync, message request response).
    pub(crate) blocked: bool,
    /// In the phone's address book (Desktop's `isInSystemContacts`): a Storage Service system
    /// name, or a name from contact sync.
    pub(crate) system_contact: bool,
}

impl Peer {
    /// Whether the 1:1 chat is accepted (Desktop's `isTrustedContact` for a direct chat): we
    /// share our profile with them, or they are in our phone's address book.
    pub(crate) fn accepted(&self) -> bool {
        self.profile_sharing || self.system_contact
    }
}

impl Default for Peer {
    fn default() -> Self {
        Self {
            access_key: None,
            sealed: SealedSenderState::Unknown,
            profile_sharing: false,
            share_my_number: false,
            expire_timer_version: 0,
            expire_timer: 0,
            unregistered: false,
            features_hash: None,
            blocked: false,
            system_contact: false,
        }
    }
}

/// Loads the row of `service_id` (a service id string).
pub(crate) fn load(conn: &Connection, service_id: &str) -> Result<Peer> {
    let row = conn
        .prepare_cached(
            "SELECT access_key, sealed_sender, profile_sharing, share_my_number,
               expire_timer_version, unregistered_at IS NOT NULL, features_hash, blocked,
               system_contact OR in_contact_sync != 0, expire_timer
             FROM peers WHERE service_id = ?1",
        )?
        .query_row([service_id], |r| {
            Ok((
                (
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                ),
                (
                    r.get::<_, i64>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, bool>(7)?,
                    r.get::<_, bool>(8)?,
                    r.get::<_, i64>(9)?,
                ),
            ))
        })
        .optional()?;
    let Some((
        (access_key, sealed, profile_sharing, share_my_number),
        (version, unregistered, hash, blocked, system_contact, timer),
    )) = row
    else {
        return Ok(Peer::default());
    };
    Ok(Peer {
        access_key: access_key.and_then(|k| <[u8; ACCESS_KEY_LEN]>::try_from(k.as_slice()).ok()),
        sealed: SealedSenderState::from_i64(sealed),
        profile_sharing,
        share_my_number,
        expire_timer_version: u32::try_from(version).unwrap_or(0),
        expire_timer: u32::try_from(timer).unwrap_or(0),
        unregistered,
        features_hash: hash,
        blocked,
        system_contact,
    })
}

/// The stored profile key of `service_id`, if any.
pub(crate) fn profile_key(
    conn: &Connection,
    service_id: &str,
) -> Result<Option<Zeroizing<[u8; 32]>>> {
    let key: Option<Option<Vec<u8>>> = conn
        .prepare_cached("SELECT profile_key FROM peers WHERE service_id = ?1")?
        .query_row([service_id], |r| r.get(0))
        .optional()?;
    Ok(key
        .flatten()
        .map(Zeroizing::new)
        .and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok().map(Zeroizing::new)))
}

/// Sets or clears "blocked". Returns whether it changed.
pub(crate) fn set_blocked(
    conn: &Connection,
    service_id: &str,
    blocked: bool,
    now: i64,
) -> Result<bool> {
    ensure(conn, service_id, now)?;
    let changed = conn
        .prepare_cached(
            "UPDATE peers SET blocked = ?2, updated_at = ?3 WHERE service_id = ?1 AND blocked != ?2",
        )?
        .execute((service_id, blocked, now))?;
    Ok(changed > 0)
}

/// Sets or clears "in the phone's address book". Returns whether it changed.
pub(crate) fn set_system_contact(
    conn: &Connection,
    service_id: &str,
    system_contact: bool,
    now: i64,
) -> Result<bool> {
    ensure(conn, service_id, now)?;
    let changed = conn
        .prepare_cached(
            "UPDATE peers SET system_contact = ?2, updated_at = ?3
             WHERE service_id = ?1 AND system_contact != ?2",
        )?
        .execute((service_id, system_contact, now))?;
    Ok(changed > 0)
}

/// Stores the 1:1 chat's disappearing-message timer (seconds, 0 = off) and its version; the
/// version never goes down.
pub(crate) fn set_expire_timer(
    conn: &Connection,
    service_id: &str,
    timer: u32,
    version: u32,
    now: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO peers(service_id, expire_timer, expire_timer_version, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(service_id) DO UPDATE SET expire_timer = excluded.expire_timer,
           expire_timer_version = max(expire_timer_version, excluded.expire_timer_version),
           updated_at = excluded.updated_at",
    )?
    .execute((service_id, timer, version, now))?;
    Ok(())
}

/// Sets the 1:1 timer and its version as the phone states them at the initial contact sync
/// (Desktop resets its version there: the phone's counter may have started over).
pub(crate) fn reset_expire_timer(
    conn: &Connection,
    service_id: &str,
    timer: u32,
    version: u32,
    now: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO peers(service_id, expire_timer, expire_timer_version, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(service_id) DO UPDATE SET expire_timer = excluded.expire_timer,
           expire_timer_version = excluded.expire_timer_version,
           updated_at = excluded.updated_at",
    )?
    .execute((service_id, timer, version, now))?;
    Ok(())
}

/// Raises the stored 1:1 timer version to `version` (the timer itself stays).
pub(crate) fn raise_timer_version(
    conn: &Connection,
    service_id: &str,
    version: u32,
    now: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO peers(service_id, expire_timer_version, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(service_id) DO UPDATE SET
           expire_timer_version = max(expire_timer_version, excluded.expire_timer_version),
           updated_at = excluded.updated_at",
    )?
    .execute((service_id, version, now))?;
    Ok(())
}

/// Ensures a row for `service_id` exists.
pub(crate) fn ensure(conn: &Connection, service_id: &str, now: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO peers(service_id, updated_at) VALUES (?1, ?2)
         ON CONFLICT(service_id) DO NOTHING",
    )?
    .execute((service_id, now))?;
    Ok(())
}

/// `ProfileKey::derive_access_key` of zkgroup (`LS/rust/zkgroup/src/api/profiles/profile_key.rs`):
/// AES-256-GCM of 16 zero bytes under the profile key with a zero nonce, which is the first
/// AES-CTR block with counter 2. zkgroup itself is not a dependency of this crate yet.
pub(crate) fn derive_access_key(profile_key: &[u8; 32]) -> Result<[u8; ACCESS_KEY_LEN]> {
    let mut block = [0u8; ACCESS_KEY_LEN];
    let mut ctr = Aes256Ctr32::from_key(profile_key, &[0u8; 12], 2)
        .map_err(|_| Error::Internal("access key derivation"))?;
    ctr.process(&mut block);
    Ok(block)
}

/// Stores a sender's profile key (`setProfileKey`): a new key derives a new access key and
/// resets the sealed-sender state to unknown. Returns whether the key changed.
pub(crate) fn store_profile_key(conn: &Connection, aci: Aci, key: &[u8], now: i64) -> Result<bool> {
    let Ok(key) = <[u8; 32]>::try_from(key) else {
        return Ok(false);
    };
    let key = Zeroizing::new(key);
    let service_id = ServiceId::from(aci).service_id_string();
    let stored: Option<Option<Vec<u8>>> = conn
        .prepare_cached("SELECT profile_key FROM peers WHERE service_id = ?1")?
        .query_row([&service_id], |r| r.get(0))
        .optional()?;
    if stored.flatten().as_deref() == Some(&key[..]) {
        return Ok(false);
    }
    let access_key = derive_access_key(&key)?;
    conn.prepare_cached(
        "INSERT INTO peers(service_id, profile_key, access_key, sealed_sender, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4)
         ON CONFLICT(service_id) DO UPDATE SET profile_key = excluded.profile_key,
           access_key = excluded.access_key, sealed_sender = 0,
           updated_at = excluded.updated_at",
    )?
    .execute((&service_id, &key[..], &access_key[..], now))?;
    Ok(true)
}

/// Records the outcome of a sealed-sender attempt (Desktop's `handleMessageSendResult` and
/// `processError`): a refusal disables sealed sender, a success turns "unknown" into "enabled"
/// (with an access key) or "unrestricted" (with the zero key).
pub(crate) fn set_sealed_state(
    conn: &Connection,
    service_id: &str,
    state: SealedSenderState,
    now: i64,
) -> Result<()> {
    ensure(conn, service_id, now)?;
    conn.prepare_cached(
        "UPDATE peers SET sealed_sender = ?2, updated_at = ?3
         WHERE service_id = ?1 AND sealed_sender != ?2",
    )?
    .execute((service_id, state.as_i64(), now))?;
    Ok(())
}

/// Sets or clears profile sharing (an accepted 1:1 chat). Returns whether it changed.
pub(crate) fn set_profile_sharing(
    conn: &Connection,
    service_id: &str,
    sharing: bool,
    now: i64,
) -> Result<bool> {
    ensure(conn, service_id, now)?;
    let changed = conn
        .prepare_cached(
            "UPDATE peers SET profile_sharing = ?2, updated_at = ?3
             WHERE service_id = ?1 AND profile_sharing != ?2",
        )?
        .execute((service_id, sharing, now))?;
    Ok(changed > 0)
}

/// They acknowledged a message that carried our PNI signature: stop sending it
/// (`MessageReceipts.preload.ts` `deleteSentProtoBatcher`).
pub(crate) fn clear_share_my_number(conn: &Connection, service_id: &str, now: i64) -> Result<()> {
    conn.prepare_cached(
        "UPDATE peers SET share_my_number = 0, updated_at = ?2
         WHERE service_id = ?1 AND share_my_number = 1",
    )?
    .execute((service_id, now))?;
    Ok(())
}

/// The server answered 404 for them (`setUnregistered`).
pub(crate) fn mark_unregistered(conn: &Connection, service_id: &str, now: i64) -> Result<()> {
    ensure(conn, service_id, now)?;
    conn.prepare_cached(
        "UPDATE peers SET unregistered_at = ?2, updated_at = ?2
         WHERE service_id = ?1 AND unregistered_at IS NULL",
    )?
    .execute((service_id, now))?;
    Ok(())
}

/// They sent us something, so they are registered (again).
pub(crate) fn mark_registered(conn: &Connection, service_id: &str, now: i64) -> Result<()> {
    conn.prepare_cached(
        "UPDATE peers SET unregistered_at = NULL, updated_at = ?2
         WHERE service_id = ?1 AND unregistered_at IS NOT NULL",
    )?
    .execute((service_id, now))?;
    Ok(())
}

/// Remembers the hash of the features last sent to the core for the chat with `service_id`.
pub(crate) fn set_features_hash(
    conn: &Connection,
    service_id: &str,
    hash: i64,
    now: i64,
) -> Result<()> {
    ensure(conn, service_id, now)?;
    conn.prepare_cached("UPDATE peers SET features_hash = ?2 WHERE service_id = ?1")?
        .execute((service_id, hash))?;
    Ok(())
}

/// Forgets that the core has this peer's chat features (the next
/// [`crate::features::features_event`] sends them again).
pub(crate) fn clear_features_hash(conn: &Connection, service_id: &str) -> Result<()> {
    conn.prepare_cached("UPDATE peers SET features_hash = NULL WHERE service_id = ?1")?
        .execute([service_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, kourier_db::ZoneDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([8; 32]),
        )
        .unwrap();
        (dir, db)
    }

    /// zkgroup's vector (`ZkGroupTest.testDeriveAccessKey`, Signal's Java tests): profile key
    /// 32 × 0x02.
    #[test]
    fn access_keys_match_zkgroup() {
        let key = derive_access_key(&[2; 32]).unwrap();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "5a723acee52c5ea02b92a3a360c09595");
    }

    #[test]
    fn a_new_profile_key_resets_sealed_sender() {
        let (_dir, db) = db();
        let aci = Aci::from(uuid::Uuid::from_u128(7));
        let sid = ServiceId::from(aci).service_id_string();
        db.zone(|tx| -> Result<()> {
            assert_eq!(load(tx, &sid)?, Peer::default());
            assert!(store_profile_key(tx, aci, &[2; 32], 1)?);
            assert!(!store_profile_key(tx, aci, &[2; 32], 2)?, "same key");
            assert!(
                !store_profile_key(tx, aci, &[2; 3], 2)?,
                "wrong length ignored"
            );
            let peer = load(tx, &sid)?;
            assert_eq!(peer.access_key, Some(derive_access_key(&[2; 32])?));
            assert_eq!(peer.sealed, SealedSenderState::Unknown);
            set_sealed_state(tx, &sid, SealedSenderState::Disabled, 3)?;
            assert_eq!(load(tx, &sid)?.sealed, SealedSenderState::Disabled);
            assert!(store_profile_key(tx, aci, &[3; 32], 4)?);
            let peer = load(tx, &sid)?;
            assert_eq!(
                peer.sealed,
                SealedSenderState::Unknown,
                "new key, new chance"
            );
            assert_eq!(peer.access_key, Some(derive_access_key(&[3; 32])?));

            assert!(set_profile_sharing(tx, &sid, true, 5)?);
            assert!(!set_profile_sharing(tx, &sid, true, 6)?);
            assert!(load(tx, &sid)?.profile_sharing);
            mark_unregistered(tx, &sid, 7)?;
            assert!(load(tx, &sid)?.unregistered);
            mark_registered(tx, &sid, 8)?;
            assert!(!load(tx, &sid)?.unregistered);
            set_features_hash(tx, &sid, -5, 9)?;
            assert_eq!(load(tx, &sid)?.features_hash, Some(-5));
            Ok(())
        })
        .unwrap();
    }
}
