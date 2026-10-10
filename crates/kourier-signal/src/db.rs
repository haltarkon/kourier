//! The Signal `backend.db` (§5.5): opening it through `kourier-db`, its migrations, and the
//! linked account row with our identities.

use std::fmt;
use std::path::Path;

use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_db::{Migrations, ZoneDb, migration};
use kourier_model::UserId;
use libsignal_core::{Aci, DeviceId, Pni, ServiceId};
use libsignal_protocol::{IdentityKey, IdentityKeyPair, PrivateKey, ProtocolAddress};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::{Error, Result};
use crate::net::AuthCredentials;
use crate::service_id::{SignalUser, aci_user_id, pni_user_id};
use crate::store::IdentityKind;

/// The Signal schema component (`schema_version.component = 'signal'`).
pub(crate) const MIGRATIONS: Migrations = Migrations::new(
    "signal",
    &[
        migration!(1, "0001_init"),
        migration!(2, "0002_jobs_remote_config"),
        migration!(3, "0003_peers"),
        migration!(4, "0004_send"),
        migration!(5, "0005_contacts_storage"),
        migration!(6, "0006_job_requests"),
        migration!(7, "0007_groups_retry"),
        migration!(8, "0008_group_send"),
        migration!(9, "0009_attachment_uploads"),
        migration!(10, "0010_modifiers"),
        migration!(11, "0011_key_maintenance"),
        migration!(12, "0012_challenged_jobs_identity_links"),
        migration!(13, "0013_account_env"),
        migration!(14, "0014_sync_requests_again"),
        migration!(15, "0015_inbox_delivered_at"),
        migration!(16, "0016_history_transfer"),
        migration!(17, "0017_history_transfer_bounds"),
        migration!(18, "0018_history_import"),
        migration!(19, "0019_history_delivery"),
    ],
);

/// Opens (creating if needed) `backend.db` with its key and brings both schema components
/// (`core_outbox` from kourier-db, then `signal`) up to date.
pub(crate) fn open(path: &Path, key: &kourier_backend_api::DbKey) -> Result<ZoneDb> {
    let key = kourier_db::DbKey::from_bytes(*key.expose());
    let mut conn = kourier_db::open(path, &key)?;
    kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS)?;
    kourier_db::migrate(&mut conn, &MIGRATIONS)?;
    Ok(ZoneDb::new(conn))
}

/// The linked account (the `account` row plus `own_identity`). `Debug` shows only log-safe
/// ids; the number, password and keys never print.
#[derive(Clone)]
pub(crate) struct Account {
    /// Our ACI.
    pub(crate) aci: Aci,
    /// Our PNI.
    pub(crate) pni: Pni,
    /// Our phone number (personal data): reported to the core with `AccountReady`.
    pub(crate) e164: String,
    /// This device.
    pub(crate) device_id: DeviceId,
    /// The device password.
    pub(crate) password: Zeroizing<String>,
    /// ACI registration id.
    pub(crate) aci_registration_id: u32,
    /// PNI registration id.
    pub(crate) pni_registration_id: u32,
    /// Our profile key (sent in every DataMessage, §6.7).
    pub(crate) profile_key: Zeroizing<[u8; 32]>,
    /// ACI identity key pair.
    pub(crate) aci_identity: IdentityKeyPair,
    /// PNI identity key pair.
    pub(crate) pni_identity: IdentityKeyPair,
    /// The post-link steps (§6.5 steps 7-8) finished and `AccountReady` was written.
    pub(crate) post_link_done: bool,
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("aci", &SignalUser::Aci(self.aci))
            .field("pni", &SignalUser::Pni(self.pni))
            .field("device_id", &u32::from(self.device_id))
            .field("post_link_done", &self.post_link_done)
            .finish_non_exhaustive()
    }
}

impl Account {
    /// The identity key pair of `kind`.
    pub(crate) fn identity(&self, kind: IdentityKind) -> &IdentityKeyPair {
        match kind {
            IdentityKind::Aci => &self.aci_identity,
            IdentityKind::Pni => &self.pni_identity,
        }
    }

    /// The registration id of `kind`.
    pub(crate) fn registration_id(&self, kind: IdentityKind) -> u32 {
        match kind {
            IdentityKind::Aci => self.aci_registration_id,
            IdentityKind::Pni => self.pni_registration_id,
        }
    }

    /// Our service id of `kind`.
    pub(crate) fn service_id(&self, kind: IdentityKind) -> ServiceId {
        match kind {
            IdentityKind::Aci => self.aci.into(),
            IdentityKind::Pni => self.pni.into(),
        }
    }

    /// Which of our identities `service_id` is, if any.
    pub(crate) fn kind_of(&self, service_id: ServiceId) -> Option<IdentityKind> {
        IdentityKind::ALL
            .into_iter()
            .find(|kind| self.service_id(*kind) == service_id)
    }

    /// Our protocol address for identity `kind` (this device).
    pub(crate) fn address(&self, kind: IdentityKind) -> ProtocolAddress {
        ProtocolAddress::new(self.service_id(kind).service_id_string(), self.device_id)
    }

    /// Credentials of the authenticated chat socket.
    pub(crate) fn credentials(&self) -> AuthCredentials {
        AuthCredentials {
            aci: self.aci,
            device_id: self.device_id,
            password: self.password.clone(),
        }
    }

    /// `[aci:…, pni:…]` for `AccountReady`.
    pub(crate) fn own_ids(&self) -> Result<Vec<UserId>> {
        let ids = [aci_user_id(self.aci), pni_user_id(self.pni)];
        ids.into_iter()
            .map(|id| id.map_err(|_| Error::Internal("own service id does not form a user id")))
            .collect()
    }

    /// Our number (E.164) for `AccountReady`; `None` when the account has none stored, which
    /// leaves what the core knows.
    pub(crate) fn number(&self) -> Option<kourier_model::Redacted<String>> {
        (!self.e164.is_empty()).then(|| kourier_model::Redacted::new(self.e164.clone()))
    }

    /// Loads the account, or `None` before linking.
    pub(crate) fn load(conn: &Connection) -> Result<Option<Self>> {
        let row = conn
            .query_row(
                "SELECT aci, pni, e164, device_id, password, aci_registration_id,
                   pni_registration_id, profile_key, post_link_sync FROM account WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, u32>(3)?,
                        Zeroizing::new(r.get::<_, String>(4)?),
                        r.get::<_, u32>(5)?,
                        r.get::<_, u32>(6)?,
                        Zeroizing::new(r.get::<_, Vec<u8>>(7)?),
                        r.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((aci, pni, e164, device, password, aci_reg, pni_reg, profile_key, sync)) = row
        else {
            return Ok(None);
        };
        let uuid = |s: &str| Uuid::try_parse(s).map_err(|_| Error::Decode("account service id"));
        Ok(Some(Self {
            aci: Aci::from(uuid(&aci)?),
            pni: Pni::from(uuid(&pni)?),
            e164,
            device_id: DeviceId::try_from(device).map_err(|_| Error::Decode("device id"))?,
            password,
            aci_registration_id: aci_reg,
            pni_registration_id: pni_reg,
            profile_key: Zeroizing::new(
                <[u8; 32]>::try_from(profile_key.as_slice())
                    .map_err(|_| Error::Decode("profile key"))?,
            ),
            aci_identity: load_identity(conn, IdentityKind::Aci)?,
            pni_identity: load_identity(conn, IdentityKind::Pni)?,
            post_link_done: sync == "done",
        }))
    }
}

/// The server the linked account is bound to (`account.env`, `net::env::binding`): `None`
/// before linking, `Some(None)` for an account linked before the binding was stored.
pub(crate) fn bound_env(conn: &Connection) -> Result<Option<Option<String>>> {
    Ok(conn
        .query_row("SELECT env FROM account WHERE id = 1", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?)
}

/// Reads our identity key pair of `kind`.
fn load_identity(conn: &Connection, kind: IdentityKind) -> Result<IdentityKeyPair> {
    let (public, private): (Vec<u8>, Zeroizing<Vec<u8>>) = conn.query_row(
        "SELECT public_key, private_key FROM own_identity WHERE kind = ?1",
        [kind.as_str()],
        |r| Ok((r.get(0)?, Zeroizing::new(r.get(1)?))),
    )?;
    let public = IdentityKey::decode(&public).map_err(|_| Error::Decode("own identity key"))?;
    let private =
        PrivateKey::deserialize(&private).map_err(|_| Error::Decode("own identity key"))?;
    Ok(IdentityKeyPair::new(public, private))
}

/// The next key ids of one own identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyIds {
    /// Next one-time EC prekey id.
    pub(crate) prekey: u32,
    /// Next signed EC prekey id.
    pub(crate) signed: u32,
    /// Next Kyber prekey id.
    pub(crate) kyber: u32,
}

impl KeyIds {
    /// Largest key id (Desktop wraps its 24-bit counters, `AccountManager.preload.ts:236-252`).
    pub(crate) const MAX: u32 = 0x00FF_FFFF;

    /// Random starting ids in `1..=MAX`, as Desktop picks them.
    pub(crate) fn random<R: rand::Rng>(rng: &mut R) -> Self {
        Self {
            prekey: rng.random_range(1..=Self::MAX),
            signed: rng.random_range(1..=Self::MAX),
            kyber: rng.random_range(1..=Self::MAX),
        }
    }

    /// `id + n`, wrapping within `1..=MAX`.
    pub(crate) fn advance(id: u32, n: u32) -> u32 {
        let zero_based = (u64::from(id).saturating_sub(1) + u64::from(n)) % u64::from(Self::MAX);
        u32::try_from(zero_based).unwrap_or(0) + 1
    }

    /// Reads the counters of `kind`.
    pub(crate) fn load(conn: &Connection, kind: IdentityKind) -> Result<Self> {
        Ok(conn.query_row(
            "SELECT next_prekey_id, next_signed_prekey_id, next_kyber_prekey_id
             FROM own_identity WHERE kind = ?1",
            [kind.as_str()],
            |r| {
                Ok(Self {
                    prekey: r.get(0)?,
                    signed: r.get(1)?,
                    kyber: r.get(2)?,
                })
            },
        )?)
    }

    /// Writes the counters of `kind`.
    pub(crate) fn store(self, conn: &Connection, kind: IdentityKind) -> Result<()> {
        conn.execute(
            "UPDATE own_identity SET next_prekey_id = ?2, next_signed_prekey_id = ?3,
               next_kyber_prekey_id = ?4 WHERE kind = ?1",
            (kind.as_str(), self.prekey, self.signed, self.kyber),
        )?;
        Ok(())
    }
}

/// Inserts one of our identities with its first key ids.
pub(crate) fn insert_own_identity(
    conn: &Connection,
    kind: IdentityKind,
    identity: &IdentityKeyPair,
    ids: KeyIds,
) -> Result<()> {
    conn.execute(
        "INSERT INTO own_identity(kind, public_key, private_key, next_prekey_id,
           next_signed_prekey_id, next_kyber_prekey_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            kind.as_str(),
            &identity.identity_key().serialize()[..],
            Zeroizing::new(identity.private_key().serialize()).as_slice(),
            ids.prekey,
            ids.signed,
            ids.kyber,
        ),
    )?;
    Ok(())
}

/// The stage-1 arrival counter and catch-up boundary.
pub(crate) fn arrival_counter(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT arrival_counter FROM account WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Stores the stage-1 arrival counter.
pub(crate) fn set_arrival_counter(conn: &Connection, counter: u64) -> Result<()> {
    conn.execute(
        "UPDATE account SET arrival_counter = ?1 WHERE id = 1",
        [i64::try_from(counter).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

/// Stores the catch-up boundary of the current connection (§4.5).
pub(crate) fn set_catch_up_boundary(conn: &Connection, boundary: u64) -> Result<()> {
    conn.execute(
        "UPDATE account SET catch_up_boundary = ?1 WHERE id = 1",
        [i64::try_from(boundary).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

/// Synced account settings the send path reads (§6.7 step 6): from `SyncMessage.Configuration`
/// now, from the Storage Service `AccountRecord` in S9. Unknown settings are off, as in Desktop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SyncedSettings {
    /// Send read receipts.
    pub(crate) read_receipts: bool,
    /// Send and show typing indicators.
    pub(crate) typing_indicators: bool,
    /// Our number goes to everybody (`phoneNumberSharingMode = EVERYBODY`): 1:1 sealed sends use
    /// the sender certificate with the number.
    pub(crate) share_number_with_everybody: bool,
}

/// Reads the synced settings.
pub(crate) fn synced_settings(conn: &Connection) -> Result<SyncedSettings> {
    Ok(conn.query_row(
        "SELECT read_receipts, coalesce(typing_indicators, 0),
           coalesce(phone_number_sharing, 'nobody') = 'everybody' FROM account WHERE id = 1",
        [],
        |r| {
            Ok(SyncedSettings {
                read_receipts: r.get(0)?,
                typing_indicators: r.get(1)?,
                share_number_with_everybody: r.get(2)?,
            })
        },
    )?)
}

/// Stores the settings of a `SyncMessage.Configuration` (absent fields stay).
pub(crate) fn store_configuration(
    conn: &Connection,
    read_receipts: Option<bool>,
    typing_indicators: Option<bool>,
) -> Result<()> {
    conn.execute(
        "UPDATE account SET read_receipts = coalesce(?1, read_receipts),
           typing_indicators = coalesce(?2, typing_indicators) WHERE id = 1",
        (read_receipts, typing_indicators),
    )?;
    Ok(())
}

/// An in-memory account with fresh identities (unit tests; nothing is stored).
#[cfg(test)]
pub(crate) fn test_account() -> Account {
    let rng = &mut rand::rng();
    Account {
        aci: Aci::from(Uuid::new_v4()),
        pni: Pni::from(Uuid::new_v4()),
        e164: "+15550100".to_owned(),
        device_id: DeviceId::new(2).expect("device id"),
        password: Zeroizing::new("password".to_owned()),
        aci_registration_id: 11,
        pni_registration_id: 12,
        profile_key: Zeroizing::new([9; 32]),
        aci_identity: IdentityKeyPair::generate(rng),
        pni_identity: IdentityKeyPair::generate(rng),
        post_link_done: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ids_wrap_within_24_bits() {
        assert_eq!(KeyIds::advance(1, 1), 2);
        assert_eq!(KeyIds::advance(KeyIds::MAX, 1), 1);
        assert_eq!(KeyIds::advance(KeyIds::MAX - 1, 3), 2);
        let ids = KeyIds::random(&mut rand::rng());
        assert!((1..=KeyIds::MAX).contains(&ids.prekey));
    }

    #[test]
    fn opening_creates_both_schema_components() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([3; 32]),
        )
        .unwrap();
        let versions: Vec<(String, i64)> = db
            .read(|c| -> Result<_> {
                let mut stmt =
                    c.prepare("SELECT component, version FROM schema_version ORDER BY component")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .unwrap();
        // kourier-db owns the `core_outbox` component and its version.
        assert_eq!(versions.len(), 2, "{versions:?}");
        assert_eq!(versions[0].0, "core_outbox");
        assert!(versions[0].1 >= 1);
        assert_eq!(versions[1], ("signal".to_owned(), 19));
        assert!(db.read(Account::load).unwrap().is_none());
    }
}
