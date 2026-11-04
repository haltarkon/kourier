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
        migration!(20, "0020_group_requesting"),
        migration!(21, "0021_number_links"),
        migration!(22, "0022_device_name"),
        migration!(23, "0023_archive_base"),
        migration!(24, "0024_phone_sharing"),
        migration!(25, "0025_storage_writes"),
    ],
);

/// Opens (creating if needed) `backend.db` with its key and brings both schema components
/// (`core_outbox` from kourier-db, then `signal`) up to date. A file that carries any other
/// component (another network's backend, the core) is refused before anything is written
/// (`kourier_db::check_backend_file`): its bytes are not touched.
pub(crate) fn open(path: &Path, key: &kourier_backend_api::DbKey) -> Result<ZoneDb> {
    let key = kourier_db::DbKey::from_bytes(*key.expose());
    let mut conn = kourier_db::open(path, &key)?;
    kourier_db::check_backend_file(&conn, &MIGRATIONS)?;
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
        assert_eq!(versions[1], ("signal".to_owned(), 25));
        assert!(db.read(Account::load).unwrap().is_none());
    }

    /// Migration 0023 (A1): a profile of schema 22 opens at 23, keeps its rows, and the new
    /// base columns start as "never told" (NULL), the account's setting too.
    #[test]
    fn a_schema_22_file_opens_at_23_with_the_archive_base_unwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backend.db");
        {
            let mut conn =
                kourier_db::open(&path, &kourier_db::DbKey::from_bytes([3; 32])).unwrap();
            kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).unwrap();
            let v22 = Migrations::new(MIGRATIONS.component, &MIGRATIONS.steps[..22]);
            kourier_db::migrate(&mut conn, &v22).unwrap();
            conn.execute(
                "INSERT INTO peers(service_id, updated_at) VALUES ('old-contact', 5)",
                [],
            )
            .unwrap();
            let columns: i64 = conn
                .query_row(
                    "SELECT count(*) FROM pragma_table_info('peers')
                     WHERE name IN ('roaming_archived', 'roaming_muted_until')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(columns, 0, "schema 22 has no base columns");
        }
        let db = open(&path, &kourier_backend_api::DbKey::new([3; 32])).unwrap();
        db.read(|c| -> Result<()> {
            let version: i64 = c.query_row(
                "SELECT version FROM schema_version WHERE component = 'signal'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(version, 25, "and on to 25 (0025_storage_writes)");
            let peer = crate::peers::load(c, "old-contact")?;
            assert_eq!(peer.roaming, None, "an old contact was never told");
            let (kind, not_null, default): (String, i64, Option<String>) = c.query_row(
                "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('account')
                 WHERE name = 'keep_muted_archived'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(
                (kind.as_str(), not_null, default.as_deref()),
                ("INTEGER", 0, None),
                "keep_muted_archived is NULL (never read) until it is filled"
            );
            Ok(())
        })
        .unwrap();
        // Only 0 and 1 are a base.
        let refused = db.zone(|tx| -> Result<()> {
            tx.execute(
                "UPDATE peers SET roaming_archived = 2 WHERE service_id = 'old-contact'",
                [],
            )?;
            Ok(())
        });
        assert!(refused.is_err());
    }

    /// Migration 0024 (people pages P1): a profile of schema 23 opens at 24, keeps its rows, and
    /// `peers.phone_sharing` starts as "never fetched" (NULL); only 0 and 1 are a verdict.
    #[test]
    fn a_schema_23_file_opens_at_24_with_the_sharing_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backend.db");
        {
            let mut conn =
                kourier_db::open(&path, &kourier_db::DbKey::from_bytes([3; 32])).unwrap();
            kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).unwrap();
            let v23 = Migrations::new(MIGRATIONS.component, &MIGRATIONS.steps[..23]);
            kourier_db::migrate(&mut conn, &v23).unwrap();
            conn.execute(
                "INSERT INTO peers(service_id, updated_at) VALUES ('old-contact', 5)",
                [],
            )
            .unwrap();
        }
        let db = open(&path, &kourier_backend_api::DbKey::new([3; 32])).unwrap();
        db.read(|c| -> Result<()> {
            let version: i64 = c.query_row(
                "SELECT version FROM schema_version WHERE component = 'signal'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(version, 25, "and on to 25 (0025_storage_writes)");
            let sharing: Option<i64> = c.query_row(
                "SELECT phone_sharing FROM peers WHERE service_id = 'old-contact'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(sharing, None, "never fetched");
            // The other 0024 additions: no attempt time, no hold, the verdict pass not done.
            let tried: Option<i64> = c.query_row(
                "SELECT profile_tried_at FROM peers WHERE service_id = 'old-contact'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(tried, None);
            let holds: i64 = c.query_row("SELECT count(*) FROM job_holds", [], |r| r.get(0))?;
            assert_eq!(holds, 0);
            let (default, not_null): (String, i64) = c.query_row(
                "SELECT dflt_value, \"notnull\" FROM pragma_table_info('account')
                 WHERE name = 'number_verdicts_stated'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!((default.as_str(), not_null), ("0", 1));
            // The rule, on the migrated row: no key, so the number shows.
            assert!(crate::peers::number_shown(c, "old-contact")?);
            Ok(())
        })
        .unwrap();
        let refused = db.zone(|tx| -> Result<()> {
            tx.execute(
                "UPDATE peers SET phone_sharing = 2 WHERE service_id = 'old-contact'",
                [],
            )?;
            Ok(())
        });
        assert!(refused.is_err());
    }

    /// Migration 0025 (A10, the Storage Service write engine): a profile of schema 24 opens at 25 and
    /// keeps every row it had; the switch of the account is OFF (the lead's answer: every account, old
    /// or new), the records have no target yet (`storage::fill_targets` fills them), the generation
    /// and the intent counter start at 0, and the three new tables are empty and strict.
    #[test]
    fn a_schema_24_file_opens_at_25_with_every_row_kept_and_the_sync_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backend.db");
        {
            let mut conn =
                kourier_db::open(&path, &kourier_db::DbKey::from_bytes([3; 32])).unwrap();
            kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).unwrap();
            let v24 = Migrations::new(MIGRATIONS.component, &MIGRATIONS.steps[..24]);
            kourier_db::migrate(&mut conn, &v24).unwrap();
            conn.execute(
                "INSERT INTO account(id, aci, pni, e164, device_id, password, aci_registration_id,
                   pni_registration_id, profile_key, master_key, storage_key, manifest_version,
                   linked_at)
                 VALUES (1, 'aci', 'pni', '+10000000', 2, 'pw', 11, 12, x'09', x'09', x'09', 7, 3)",
                [],
            )
            .unwrap();
            for (key, kind, peer) in [(1, 1, Some("old-contact")), (2, 2, None), (3, 3, None)] {
                conn.execute(
                    "INSERT INTO storage_records(key, kind, peer, record, updated_at)
                     VALUES (?1, ?2, ?3, x'0a00', 5)",
                    (vec![key as u8; 16], kind, peer),
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO peers(service_id, updated_at) VALUES ('old-contact', 5)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO groups(master_key, group_id, archived, updated_at)
                 VALUES (x'07', x'08', 1, 5)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO job_holds(kind, until) VALUES ('profile_fetch', 99)",
                [],
            )
            .unwrap();
            let columns: i64 = conn
                .query_row(
                    "SELECT (SELECT count(*) FROM pragma_table_info('storage_records')
                             WHERE name = 'target')
                          + (SELECT count(*) FROM pragma_table_info('account')
                             WHERE name IN ('roaming_writes', 'storage_generation',
                                            'storage_intent_seq'))",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(columns, 0, "schema 24 has none of the new columns");
        }
        let db = open(&path, &kourier_backend_api::DbKey::new([3; 32])).unwrap();
        db.read(|c| -> Result<()> {
            let version: i64 = c.query_row(
                "SELECT version FROM schema_version WHERE component = 'signal'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(version, 25);
            // Every row of schema 24 is still there, unchanged.
            let (manifest, linked, switch, generation, intent_seq): (i64, i64, i64, i64, i64) = c
                .query_row(
                    "SELECT manifest_version, linked_at, roaming_writes, storage_generation,
                            storage_intent_seq FROM account WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?;
            assert_eq!((manifest, linked), (7, 3));
            assert_eq!(
                (switch, generation, intent_seq),
                (0, 0, 0),
                "the sync is off for an account that existed, and nothing is counted yet"
            );
            type Row = (Vec<u8>, i64, Option<String>, Vec<u8>, Option<String>);
            let records: Vec<Row> = {
                let mut stmt = c.prepare(
                    "SELECT key, kind, peer, record, target FROM storage_records ORDER BY key",
                )?;
                stmt.query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<kourier_db::rusqlite::Result<_>>()?
            };
            assert_eq!(
                records,
                [
                    (vec![1; 16], 1, Some("old-contact".to_owned()), vec![0x0a, 0], None),
                    (vec![2; 16], 2, None, vec![0x0a, 0], None),
                    (vec![3; 16], 3, None, vec![0x0a, 0], None),
                ],
                "the records are kept byte for byte and have no target until it is filled"
            );
            // Review N6: a record from before 0025 is the phone's (`ours` = 0, never NULL).
            let (phones, ours_decl): (i64, (i64, String)) = (
                c.query_row(
                    "SELECT count(*) FROM storage_records WHERE ours = 0",
                    [],
                    |r| r.get(0),
                )?,
                c.query_row(
                    "SELECT \"notnull\", dflt_value FROM pragma_table_info('storage_records')
                     WHERE name = 'ours'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?,
            );
            assert_eq!((phones, ours_decl.0, ours_decl.1.as_str()), (3, 1, "0"));
            let kept: (i64, i64, i64) = c.query_row(
                "SELECT (SELECT count(*) FROM peers WHERE service_id = 'old-contact'),
                        (SELECT archived FROM groups WHERE master_key = x'07'),
                        (SELECT until FROM job_holds WHERE kind = 'profile_fetch')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(kept, (1, 1, 99));
            // The new columns' declarations: the switch and the counters can never be NULL.
            for name in ["roaming_writes", "storage_generation", "storage_intent_seq"] {
                let (not_null, default): (i64, String) = c.query_row(
                    "SELECT \"notnull\", dflt_value FROM pragma_table_info('account') WHERE name = ?1",
                    [name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                assert_eq!((not_null, default.as_str()), (1, "0"), "{name}");
            }
            // The three new tables are empty.
            for table in [
                "storage_intents",
                "storage_write_inflight",
                "storage_write_times",
            ] {
                let rows: i64 =
                    c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
                assert_eq!(rows, 0, "{table}");
            }
            // The partial index serves a lookup by target (A10's plan reads records that way).
            let plan: String = c.query_row(
                "EXPLAIN QUERY PLAN SELECT key FROM storage_records WHERE target = ?1",
                ["account"],
                |r| r.get(3),
            )?;
            assert!(plan.contains("storage_records_target"), "{plan}");
            Ok(())
        })
        .unwrap();
        // What the schema refuses: the switch is 0 or 1, an intent's flag too, one in-flight row of
        // a valid JSON plan, strict types, one intent per target and field.
        let refuses = |sql: &str| {
            let sql = sql.to_owned();
            db.zone(move |tx| -> Result<()> {
                tx.execute(&sql, [])?;
                Ok(())
            })
            .is_err()
        };
        assert!(refuses("UPDATE account SET roaming_writes = 2"));
        assert!(refuses(
            "INSERT INTO storage_intents(target, field, seq, automatic, created_at)
             VALUES ('account', 'archived', 1, 2, 0)"
        ));
        assert!(refuses(
            "INSERT INTO storage_intents(target, field, value, seq, created_at)
             VALUES ('account', 'archived', 'yes', 1, 0)"
        ));
        assert!(refuses(
            "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation,
               planned_at) VALUES (2, 8, x'', '[]', 0, 0)"
        ));
        assert!(refuses(
            "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation,
               planned_at) VALUES (1, 8, x'', 'not json', 0, 0)"
        ));
        assert!(refuses(
            "INSERT INTO storage_write_times(at) VALUES ('later')"
        ));
        assert!(!refuses(
            "INSERT INTO storage_intents(target, field, value, base, shown, key, seq, automatic,
               created_at) VALUES ('contact:x', 'identity', 1, 0, 0, x'05', 1, 0, 0)"
        ));
        assert!(
            refuses(
                "INSERT INTO storage_intents(target, field, seq, created_at)
                 VALUES ('contact:x', 'identity', 2, 0)"
            ),
            "one intent per target and field"
        );
        assert!(!refuses(
            "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation,
               planned_at) VALUES (1, 8, x'0102', '[{\"target\":\"account\"}]', 0, 0)"
        ));
        assert!(
            refuses(
                "INSERT INTO storage_write_inflight(id, version, new_keys, planned, generation,
                   planned_at) VALUES (1, 9, x'', '[]', 0, 0)"
            ),
            "at most one write in flight"
        );
        assert!(!refuses(
            "INSERT INTO storage_write_times(at) VALUES (12345)"
        ));
    }

    /// A file that carries any component but `core_outbox` and `signal` (a WhatsApp account's file
    /// handed over by a registry mix-up, the core's, one of a backend that does not exist yet) is
    /// refused BEFORE anything is written, so the other backend can still open it: Signal must not
    /// add its tables to it. Every file of the directory is byte for byte what it was, and an outbox
    /// newer than this build does not answer `TooNew` first.
    #[test]
    fn a_file_of_another_backend_is_refused_before_any_write() {
        let outbox = i64::from(kourier_db::outbox::MIGRATIONS.latest());
        let key = || kourier_backend_api::DbKey::new([3; 32]);
        let cases = [
            ("whatsapp", Some(outbox), "whatsapp"),
            ("the core", None, "core"),
            ("a backend of the future", None, "threema"),
            (
                "whatsapp with a newer outbox",
                Some(outbox + 50),
                "whatsapp",
            ),
        ];
        for (case, outbox, other) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("backend.db");
            {
                let mut conn =
                    kourier_db::open(&path, &kourier_db::DbKey::from_bytes([3; 32])).unwrap();
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS schema_version(
                       component TEXT PRIMARY KEY, version INTEGER NOT NULL)",
                )
                .unwrap();
                if let Some(version) = outbox {
                    kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).unwrap();
                    conn.execute(
                        "UPDATE schema_version SET version = ?1 WHERE component = 'core_outbox'",
                        [version],
                    )
                    .unwrap();
                }
                conn.execute(
                    "INSERT INTO schema_version(component, version) VALUES (?1, 1)",
                    [other],
                )
                .unwrap();
            }
            let snapshot = || {
                let mut files: Vec<_> = std::fs::read_dir(dir.path())
                    .unwrap()
                    .map(|e| {
                        let path = e.unwrap().path();
                        (path.clone(), std::fs::read(&path).unwrap())
                    })
                    .collect();
                files.sort();
                files
            };
            let before = snapshot();
            match open(&path, &key()) {
                Err(Error::Db(kourier_db::Error::ForeignComponent { component })) => {
                    assert_eq!(component, other, "{case}");
                }
                Err(other) => panic!("{case}: refused with another error: {other}"),
                Ok(_) => panic!("{case}: the foreign file was opened"),
            }
            assert!(before == snapshot(), "{case}: the foreign file was written");
        }
    }
}
