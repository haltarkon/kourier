//! Our prekeys: generation, the JSON forms of the keys API, upload, refill and rotation (§6.5
//! steps 4 and 7, §6.12 key maintenance; ports `SD/ts/textsecure/AccountManager.preload.ts`
//! `maybeUpdateKeys`, `_confirmKeys` and the `_clean*` functions, `UpdateKeysListener.preload.ts`
//! and `SD/ts/Curve.node.ts`), and parsing of other users' prekey bundles for the send path.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::Method;
use kourier_backend_api::clock::duration_ms;
use kourier_db::ZoneDb;
use kourier_db::rusqlite::Connection;
use libsignal_core::DeviceId;
use libsignal_protocol::{
    GenericSignedPreKey as _, IdentityKey, IdentityKeyPair, KeyPair, KyberPreKeyRecord,
    PreKeyBundle, PreKeyRecord, PublicKey, SignedPreKeyRecord, Timestamp, kem,
};
use rand::{CryptoRng, Rng};
use serde::{Deserialize, Serialize};

use crate::db::{Account, KeyIds};
use crate::error::{Error, Result};
use crate::net::{ChatRequest, Net, SignalTransport, json_request, parse_json};
use crate::store::{IdentityKind, insert_kyber_prekey, insert_prekey, insert_signed_prekey};

/// One-time keys generated per refill and identity (Desktop's `PRE_KEY_GEN_BATCH_SIZE`).
pub(crate) const ONE_TIME_BATCH: u32 = 100;

/// Refill when the server holds fewer one-time keys than this (`PRE_KEY_MINIMUM`, §6.12).
const REFILL_BELOW: u64 = 10;

/// Refill also when the server holds more than this (`PRE_KEY_MAX_COUNT`): "unlikely, but has
/// happened due to bugs", and an upload replaces the server's whole set.
const REFILL_ABOVE: u64 = 200;

/// A new signed prekey and Kyber last-resort key once the newest one the server has is this old
/// (`SIGNED_PRE_KEY_ROTATION_AGE`, `LAST_RESORT_KEY_ROTATION_AGE`: 1.5 days).
pub(crate) const ROTATION_AGE: Duration = Duration::from_secs(36 * 60 * 60);

/// How often key maintenance runs (`UpdateKeysListener` `UPDATE_INTERVAL`: 2 days), besides
/// every start.
pub(crate) const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// Signed prekeys and last-resort keys always kept, newest first (`SIGNED_PRE_KEY_MINIMUM`,
/// `LAST_RESORT_KEY_MINIMUM`); older ones go once no queued message can still use them.
const KEEP_SIGNED: i64 = 5;

/// One-time keys always kept, newest first (`PRE_KEY_MAX_COUNT`); older ones go after
/// [`ONE_TIME_ARCHIVE_AGE`].
const KEEP_ONE_TIME: i64 = 200;

/// `PRE_KEY_ARCHIVE_AGE`: 90 days.
const ONE_TIME_ARCHIVE_AGE: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// The remote-config entry with how long the server keeps an undelivered message (seconds).
const QUEUE_TIME_KEY: &str = "global.messageQueueTimeInSeconds";

/// The least that time counts as (`getMessageQueueTime`: Desktop's `MONTH`, 30 days): a
/// message sealed to a rotated signed prekey may still be on its way for that long.
const QUEUE_TIME_MIN: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// `{keyId, publicKey}` of the keys API.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreKeyJson {
    /// Key id.
    pub(crate) key_id: u32,
    /// Serialized public key, base64.
    pub(crate) public_key: String,
}

/// `{keyId, publicKey, signature}` of the keys API (signed EC and Kyber keys).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SignedPreKeyJson {
    /// Key id.
    pub(crate) key_id: u32,
    /// Serialized public key, base64.
    pub(crate) public_key: String,
    /// Signature by the identity key, base64.
    pub(crate) signature: String,
}

impl SignedPreKeyJson {
    /// The JSON form of a signed EC prekey.
    pub(crate) fn signed(record: &SignedPreKeyRecord) -> Result<Self> {
        Ok(Self {
            key_id: record.id()?.into(),
            public_key: STANDARD.encode(record.public_key()?.serialize()),
            signature: STANDARD.encode(record.signature()?),
        })
    }

    /// The JSON form of a Kyber prekey.
    pub(crate) fn kyber(record: &KyberPreKeyRecord) -> Result<Self> {
        Ok(Self {
            key_id: record.id()?.into(),
            public_key: STANDARD.encode(record.public_key()?.serialize()),
            signature: STANDARD.encode(record.signature()?),
        })
    }
}

/// Body of `PUT /v2/keys?identity=…` (`registerKeys`): whichever of the four kinds of key this
/// upload brings. A kind that is absent stays as the server has it, so absent lists are left
/// out, as Desktop leaves them `undefined`.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadKeys {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pre_keys: Vec<PreKeyJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pq_pre_keys: Vec<SignedPreKeyJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pq_last_resort_pre_key: Option<SignedPreKeyJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signed_pre_key: Option<SignedPreKeyJson>,
}

/// Answer of `GET /v2/keys?identity=…`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeyCounts {
    /// One-time EC prekeys the server holds.
    pub(crate) count: u64,
    /// One-time Kyber prekeys the server holds.
    pub(crate) pq_count: u64,
}

/// How much one maintenance run may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// One-time keys only: every connect, and the upload that completes a link (§6.5 step 7).
    OneTime,
    /// One-time keys by the server's counts, a new signed prekey and last-resort key by age
    /// (the routine run: at start and every 2 days).
    Routine,
    /// Everything new whatever the server holds (after a number change, Desktop's
    /// `forceUpdate`).
    Forced,
}

/// What one run uploads for one identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// A batch of one-time EC prekeys.
    pub(crate) one_time: bool,
    /// A batch of one-time Kyber prekeys.
    pub(crate) kyber: bool,
    /// A new signed EC prekey.
    pub(crate) signed: bool,
    /// A new Kyber last-resort prekey.
    pub(crate) last_resort: bool,
}

impl Plan {
    /// Nothing to upload.
    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// When the newest signed prekey and last-resort key the server has were made (ms).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Newest {
    /// Newest confirmed signed EC prekey.
    pub(crate) signed: Option<i64>,
    /// Newest confirmed Kyber last-resort prekey.
    pub(crate) last_resort: Option<i64>,
}

impl Newest {
    /// Reads them for `kind`.
    pub(crate) fn load(conn: &Connection, kind: IdentityKind) -> Result<Self> {
        let signed = conn.query_row(
            "SELECT max(created_at) FROM signed_prekeys WHERE identity = ?1 AND confirmed = 1",
            [kind.as_str()],
            |r| r.get(0),
        )?;
        let last_resort = conn.query_row(
            "SELECT max(created_at) FROM kyber_prekeys
             WHERE identity = ?1 AND last_resort = 1 AND confirmed = 1",
            [kind.as_str()],
            |r| r.get(0),
        )?;
        Ok(Self {
            signed,
            last_resort,
        })
    }
}

/// Desktop's decisions in `maybeUpdateKeys`: one-time keys of a kind are replaced when the
/// server has fewer than 10 or more than 200 of them, the signed prekey and the last-resort
/// key when the newest confirmed one is 1.5 days old (or there is none).
pub(crate) fn plan(mode: Mode, counts: KeyCounts, newest: Newest, now: i64) -> Plan {
    let force = mode == Mode::Forced;
    let out_of_range = |count: u64| !(REFILL_BELOW..=REFILL_ABOVE).contains(&count);
    let age = i64::try_from(duration_ms(ROTATION_AGE)).unwrap_or(i64::MAX);
    let old = |made: Option<i64>| made.is_none_or(|made| now.saturating_sub(made) >= age);
    let rotate = mode != Mode::OneTime;
    Plan {
        one_time: force || out_of_range(counts.count),
        kyber: force || out_of_range(counts.pq_count),
        signed: force || (rotate && old(newest.signed)),
        last_resort: force || (rotate && old(newest.last_resort)),
    }
}

/// A fresh signed EC prekey (`SD/ts/Curve.node.ts:21-41`).
pub(crate) fn generate_signed_prekey<R: Rng + CryptoRng>(
    identity: &IdentityKeyPair,
    id: u32,
    now: Timestamp,
    rng: &mut R,
) -> Result<SignedPreKeyRecord> {
    let pair = KeyPair::generate(rng);
    let signature = identity
        .private_key()
        .calculate_signature(&pair.public_key.serialize(), rng)
        .map_err(|_| Error::Internal("signing a prekey failed"))?;
    Ok(SignedPreKeyRecord::new(id.into(), now, &pair, &signature))
}

/// A fresh Kyber-1024 prekey signed by the identity key (`SD/ts/Curve.node.ts:56-72`).
pub(crate) fn generate_kyber_prekey(
    identity: &IdentityKeyPair,
    id: u32,
) -> Result<KyberPreKeyRecord> {
    Ok(KyberPreKeyRecord::generate(
        kem::KeyType::Kyber1024,
        id.into(),
        identity.private_key(),
    )?)
}

/// The keys of one upload, stored and not yet confirmed.
struct Prepared {
    body: UploadKeys,
    signed_id: Option<u32>,
    last_resort_id: Option<u32>,
    one_time: u32,
}

/// Generates what `plan` asks for, stores it and the advanced key ids in this zone, and returns
/// the upload body. The keys are durable before the server can hand them out (a crash after
/// the upload would otherwise leave unusable keys there); a signed prekey or last-resort key
/// stays unconfirmed until [`confirm_and_clean`].
fn prepare<R: Rng + CryptoRng>(
    conn: &Connection,
    account: &Account,
    kind: IdentityKind,
    plan: Plan,
    now: i64,
    rng: &mut R,
) -> Result<Prepared> {
    let identity = account.identity(kind);
    let mut ids = KeyIds::load(conn, kind)?;
    let mut body = UploadKeys::default();
    if plan.one_time {
        for i in 0..ONE_TIME_BATCH {
            let record = PreKeyRecord::new(
                KeyIds::advance(ids.prekey, i).into(),
                &KeyPair::generate(rng),
            );
            insert_prekey(conn, kind, &record, now)?;
            body.pre_keys.push(PreKeyJson {
                key_id: record.id()?.into(),
                public_key: STANDARD.encode(record.public_key()?.serialize()),
            });
        }
        ids.prekey = KeyIds::advance(ids.prekey, ONE_TIME_BATCH);
    }
    if plan.kyber {
        for i in 0..ONE_TIME_BATCH {
            let kyber = generate_kyber_prekey(identity, KeyIds::advance(ids.kyber, i))?;
            insert_kyber_prekey(conn, kind, &kyber, false, false, now)?;
            body.pq_pre_keys.push(SignedPreKeyJson::kyber(&kyber)?);
        }
        ids.kyber = KeyIds::advance(ids.kyber, ONE_TIME_BATCH);
    }
    let mut last_resort_id = None;
    if plan.last_resort {
        // Last-resort keys share the Kyber id counter (`KYBER_KEY_ID_KEY`).
        let kyber = generate_kyber_prekey(identity, ids.kyber)?;
        insert_kyber_prekey(conn, kind, &kyber, true, false, now)?;
        body.pq_last_resort_pre_key = Some(SignedPreKeyJson::kyber(&kyber)?);
        last_resort_id = Some(ids.kyber);
        ids.kyber = KeyIds::advance(ids.kyber, 1);
    }
    let mut signed_id = None;
    if plan.signed {
        let made = Timestamp::from_epoch_millis(u64::try_from(now).unwrap_or(0));
        let signed = generate_signed_prekey(identity, ids.signed, made, rng)?;
        insert_signed_prekey(conn, kind, &signed, false)?;
        body.signed_pre_key = Some(SignedPreKeyJson::signed(&signed)?);
        signed_id = Some(ids.signed);
        ids.signed = KeyIds::advance(ids.signed, 1);
    }
    ids.store(conn, kind)?;
    let one_time = if plan.one_time || plan.kyber {
        ONE_TIME_BATCH
    } else {
        0
    };
    Ok(Prepared {
        body,
        signed_id,
        last_resort_id,
        one_time,
    })
}

/// How long a rotated signed prekey or last-resort key may still be needed: the server's
/// message queue time from the remote config, at least 30 days (`getMessageQueueTime`).
fn queue_time(conn: &Connection) -> Result<Duration> {
    let configured = configured_queue_time(conn)?;
    Ok(configured.map_or(QUEUE_TIME_MIN, |time| time.max(QUEUE_TIME_MIN)))
}

/// The server's message queue time as the stored remote config states it (`None`: no
/// config stored yet, or no such value), without the floor.
pub(crate) fn configured_queue_time(conn: &Connection) -> Result<Option<Duration>> {
    Ok(
        crate::maintenance::remote_config_value(conn, QUEUE_TIME_KEY)?
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs),
    )
}

/// After the server accepted an upload (`_confirmKeys`, then the `_clean*` functions): marks
/// its signed prekey and last-resort key confirmed, and deletes keys nothing can use any more:
/// signed prekeys and last-resort keys beyond the newest five once older than the server's
/// message queue time, one-time keys beyond the newest 200 once older than 90 days. Returns
/// how many keys it deleted.
pub(crate) fn confirm_and_clean(
    conn: &Connection,
    kind: IdentityKind,
    signed_id: Option<u32>,
    last_resort_id: Option<u32>,
    now: i64,
) -> Result<usize> {
    if let Some(id) = signed_id {
        conn.execute(
            "UPDATE signed_prekeys SET confirmed = 1 WHERE identity = ?1 AND key_id = ?2",
            (kind.as_str(), id),
        )?;
    }
    if let Some(id) = last_resort_id {
        conn.execute(
            "UPDATE kyber_prekeys SET confirmed = 1
             WHERE identity = ?1 AND key_id = ?2 AND last_resort = 1",
            (kind.as_str(), id),
        )?;
    }
    let before =
        |age: Duration| now.saturating_sub(i64::try_from(duration_ms(age)).unwrap_or(i64::MAX));
    let queued = before(queue_time(conn)?);
    let archived = before(ONE_TIME_ARCHIVE_AGE);
    let mut deleted = conn.execute(
        "DELETE FROM signed_prekeys WHERE identity = ?1 AND created_at < ?2 AND key_id NOT IN
           (SELECT key_id FROM signed_prekeys WHERE identity = ?1
            ORDER BY created_at DESC, key_id DESC LIMIT ?3)",
        (kind.as_str(), queued, KEEP_SIGNED),
    )?;
    let gone_last_resort: Vec<u32> = conn
        .prepare(
            "DELETE FROM kyber_prekeys WHERE identity = ?1 AND last_resort = 1
               AND created_at < ?2 AND key_id NOT IN
               (SELECT key_id FROM kyber_prekeys WHERE identity = ?1 AND last_resort = 1
                ORDER BY created_at DESC, key_id DESC LIMIT ?3)
             RETURNING key_id",
        )?
        .query_map((kind.as_str(), queued, KEEP_SIGNED), |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    for id in &gone_last_resort {
        // The replay guard of a key that no longer exists.
        conn.execute(
            "DELETE FROM kyber_triples WHERE identity = ?1 AND kyber_id = ?2",
            (kind.as_str(), id),
        )?;
    }
    deleted += gone_last_resort.len();
    deleted += conn.execute(
        "DELETE FROM prekeys WHERE identity = ?1 AND created_at < ?2 AND key_id NOT IN
           (SELECT key_id FROM prekeys WHERE identity = ?1
            ORDER BY created_at DESC, key_id DESC LIMIT ?3)",
        (kind.as_str(), archived, KEEP_ONE_TIME),
    )?;
    deleted += conn.execute(
        "DELETE FROM kyber_prekeys WHERE identity = ?1 AND last_resort = 0
           AND created_at < ?2 AND key_id NOT IN
           (SELECT key_id FROM kyber_prekeys WHERE identity = ?1 AND last_resort = 0
            ORDER BY created_at DESC, key_id DESC LIMIT ?3)",
        (kind.as_str(), archived, KEEP_ONE_TIME),
    )?;
    Ok(deleted)
}

/// What one maintenance run uploaded for one identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Uploaded {
    /// One-time keys per kind uploaded (0 or [`ONE_TIME_BATCH`]).
    pub(crate) one_time: u32,
    /// A new signed prekey went up.
    pub(crate) signed: bool,
    /// A new last-resort key went up.
    pub(crate) last_resort: bool,
}

/// Makes sure the server holds one-time keys for both identities: reads the counts and uploads
/// a fresh batch where they are out of range. Right after linking this is the upload that
/// completes provisioning (§6.5 step 7). A PNI failure is tolerated, as in Desktop; the routine
/// run (`maintenance`) decides what a refusal of our PNI keys means.
pub(crate) async fn ensure_one_time_keys<T: SignalTransport>(
    net: &Net<T>,
    socket: &Arc<T::Socket>,
    db: &ZoneDb,
    account: &Account,
    now: i64,
) -> Result<()> {
    for kind in IdentityKind::ALL {
        match maintain(net, socket, db, account, kind, Mode::OneTime, now).await {
            Ok(uploaded) => {
                tracing::info!(identity = %kind, uploaded = uploaded.one_time, "one-time prekeys checked");
            }
            Err(e) if kind == IdentityKind::Pni => {
                tracing::warn!(error = %e, "PNI prekey upload failed; retried at next connect");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// One maintenance run for identity `kind` (`maybeUpdateKeys`): the server's counts, the plan,
/// one upload with everything the plan asks for, then confirmation and cleanup.
pub(crate) async fn maintain<T: SignalTransport>(
    net: &Net<T>,
    socket: &Arc<T::Socket>,
    db: &ZoneDb,
    account: &Account,
    kind: IdentityKind,
    mode: Mode,
    now: i64,
) -> Result<Uploaded> {
    let path = format!("/v2/keys?identity={kind}");
    let response = net.request(socket, ChatRequest::get(&path)?).await?;
    if !response.status.is_success() {
        return Err(Error::Status {
            what: "key counts",
            status: response.status.as_u16(),
        });
    }
    let counts: KeyCounts = parse_json(&response)?;
    let plan = plan(mode, counts, db.read(|c| Newest::load(c, kind))?, now);
    if plan.is_empty() {
        return Ok(Uploaded::default());
    }
    let prepared = db.zone(|tx| prepare(tx, account, kind, plan, now, &mut rand::rng()))?;
    let response = net
        .request(socket, json_request(Method::PUT, &path, &prepared.body)?)
        .await?;
    if !response.status.is_success() {
        return Err(Error::Status {
            what: "key upload",
            status: response.status.as_u16(),
        });
    }
    let deleted = db
        .zone(|tx| confirm_and_clean(tx, kind, prepared.signed_id, prepared.last_resort_id, now))?;
    if deleted > 0 {
        tracing::info!(identity = %kind, deleted, "old prekeys deleted");
    }
    Ok(Uploaded {
        one_time: prepared.one_time,
        signed: prepared.signed_id.is_some(),
        last_resort: prepared.last_resort_id.is_some(),
    })
}

/// One device's keys in `GET /v2/keys/{serviceId}/{deviceId|*}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceKeysJson {
    device_id: u32,
    registration_id: u32,
    signed_pre_key: SignedPreKeyJson,
    #[serde(default)]
    pq_pre_key: Option<SignedPreKeyJson>,
    #[serde(default)]
    pre_key: Option<PreKeyJson>,
}

/// `GET /v2/keys/{serviceId}/{deviceId|*}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreKeyResponseJson {
    identity_key: String,
    devices: Vec<DeviceKeysJson>,
}

/// A base64 value of a server answer (padded or not, see [`crate::wire_b64`]).
fn b64(field: &'static str, value: &str) -> Result<Vec<u8>> {
    crate::wire_b64::LENIENT
        .decode(value)
        .map_err(|_| Error::Decode(field))
}

/// Parses a prekey response into one bundle per device. A device without a Kyber key is
/// refused: PQXDH is mandatory (§6.7).
pub(crate) fn parse_bundles(body: &[u8]) -> Result<Vec<(DeviceId, PreKeyBundle)>> {
    let response: PreKeyResponseJson =
        serde_json::from_slice(body).map_err(|_| Error::Decode("prekey response"))?;
    let identity = IdentityKey::decode(&b64("identity key", &response.identity_key)?)
        .map_err(|_| Error::Decode("identity key"))?;
    response
        .devices
        .into_iter()
        .map(|device| {
            let device_id =
                DeviceId::try_from(device.device_id).map_err(|_| Error::Decode("device id"))?;
            let pre_key = device
                .pre_key
                .map(|key| -> Result<_> {
                    let public = PublicKey::deserialize(&b64("prekey", &key.public_key)?)
                        .map_err(|_| Error::Decode("prekey"))?;
                    Ok((key.key_id.into(), public))
                })
                .transpose()?;
            let signed = device.signed_pre_key;
            let signed_public = PublicKey::deserialize(&b64("signed prekey", &signed.public_key)?)
                .map_err(|_| Error::Decode("signed prekey"))?;
            let kyber = device
                .pq_pre_key
                .ok_or(Error::Decode("Kyber prekey (missing)"))?;
            let kyber_public =
                kem::PublicKey::deserialize(&b64("Kyber prekey", &kyber.public_key)?)
                    .map_err(|_| Error::Decode("Kyber prekey"))?;
            let bundle = PreKeyBundle::new(
                device.registration_id,
                device_id,
                pre_key,
                signed.key_id.into(),
                signed_public,
                b64("signed prekey signature", &signed.signature)?,
                kyber.key_id.into(),
                kyber_public,
                b64("Kyber prekey signature", &kyber.signature)?,
                identity,
            )?;
            Ok((device_id, bundle))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receive::tests::fixture;

    const DAY: i64 = 24 * 60 * 60 * 1000;
    /// Ten days after the keys of the test fixture were made.
    const NOW: i64 = 1_700_000_000_123 + 10 * DAY;

    /// §6.12, Desktop's `maybeUpdateKeys`: one-time keys by the server's counts (fewer than 10
    /// or more than 200), the signed prekey and the last-resort key by the age of the newest
    /// confirmed one (1.5 days), everything when forced.
    #[test]
    fn the_plan_follows_the_servers_counts_and_the_keys_ages() {
        let counts = |count, pq_count| KeyCounts { count, pq_count };
        let fresh = Newest {
            signed: Some(NOW - DAY),
            last_resort: Some(NOW - DAY),
        };
        let nothing = Plan::default();
        assert!(nothing.is_empty());
        assert_eq!(plan(Mode::Routine, counts(100, 100), fresh, NOW), nothing);
        assert_eq!(plan(Mode::Routine, counts(10, 200), fresh, NOW), nothing);
        // Each kind of one-time key on its own count.
        assert_eq!(
            plan(Mode::Routine, counts(9, 100), fresh, NOW),
            Plan {
                one_time: true,
                ..nothing
            }
        );
        assert_eq!(
            plan(Mode::Routine, counts(100, 0), fresh, NOW),
            Plan {
                kyber: true,
                ..nothing
            }
        );
        assert_eq!(
            plan(Mode::Routine, counts(201, 201), fresh, NOW),
            Plan {
                one_time: true,
                kyber: true,
                ..nothing
            },
            "too many on the server: an upload replaces them"
        );
        // Rotation at 1.5 days, each key by its own age; no confirmed key counts as old.
        let half = DAY / 2;
        let aged = |signed, last_resort| Newest {
            signed,
            last_resort,
        };
        assert_eq!(
            plan(
                Mode::Routine,
                counts(100, 100),
                aged(Some(NOW - DAY - half), Some(NOW - DAY - half + 1)),
                NOW
            ),
            Plan {
                signed: true,
                ..nothing
            }
        );
        assert_eq!(
            plan(Mode::Routine, counts(100, 100), aged(Some(NOW), None), NOW),
            Plan {
                last_resort: true,
                ..nothing
            }
        );
        // The check at every connect never rotates.
        assert_eq!(
            plan(Mode::OneTime, counts(0, 100), aged(None, None), NOW),
            Plan {
                one_time: true,
                ..nothing
            }
        );
        // After a number change: everything, whatever the server holds.
        assert_eq!(
            plan(Mode::Forced, counts(100, 100), fresh, NOW),
            Plan {
                one_time: true,
                kyber: true,
                signed: true,
                last_resort: true,
            }
        );
    }

    fn rows(db: &ZoneDb, sql: &str) -> Vec<(u32, bool)> {
        db.read(|c| -> Result<_> {
            let mut stmt = c.prepare(sql)?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?)
        })
        .unwrap()
    }

    /// A rotation: new keys are stored before the upload and are not "the server's" until it
    /// succeeded; the upload names only what it brings; ids move on; the keys the server had
    /// before stay usable for messages already on their way.
    #[test]
    fn a_rotation_is_stored_first_and_confirmed_after_the_upload() {
        let fixture = fixture();
        let kind = IdentityKind::Aci;
        let before = fixture.db.read(|c| KeyIds::load(c, kind)).unwrap();
        let old = fixture.db.read(|c| Newest::load(c, kind)).unwrap();
        let rotate = Plan {
            signed: true,
            last_resort: true,
            ..Plan::default()
        };
        let prepared = fixture
            .db
            .zone(|tx| prepare(tx, &fixture.account, kind, rotate, NOW, &mut rand::rng()))
            .unwrap();
        assert_eq!(prepared.signed_id, Some(before.signed));
        assert_eq!(prepared.last_resort_id, Some(before.kyber));
        assert_eq!(prepared.one_time, 0);
        let json = serde_json::to_value(&prepared.body).unwrap();
        let fields: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            fields,
            ["pqLastResortPreKey", "signedPreKey"],
            "no empty one-time lists: the server keeps what it has"
        );
        assert_eq!(json["signedPreKey"]["keyId"], before.signed);
        let after = fixture.db.read(|c| KeyIds::load(c, kind)).unwrap();
        assert_eq!(after.signed, KeyIds::advance(before.signed, 1));
        assert_eq!(after.kyber, KeyIds::advance(before.kyber, 1));
        assert_eq!(after.prekey, before.prekey);
        // Not confirmed yet: a crash here would rotate again next time.
        assert_eq!(fixture.db.read(|c| Newest::load(c, kind)).unwrap(), old);

        fixture
            .db
            .zone(|tx| {
                confirm_and_clean(tx, kind, prepared.signed_id, prepared.last_resort_id, NOW)
            })
            .unwrap();
        let newest = fixture.db.read(|c| Newest::load(c, kind)).unwrap();
        assert_eq!(newest.signed, Some(NOW));
        assert_eq!(newest.last_resort, Some(NOW));
        // The fixture's keys (ids 1 and 2) are still there.
        let signed = rows(
            &fixture.db,
            "SELECT key_id, confirmed FROM signed_prekeys WHERE identity = 'aci' ORDER BY created_at",
        );
        assert_eq!(signed.len(), 2);
        assert!(signed.iter().all(|(_, confirmed)| *confirmed));
        // The identity signed what goes up.
        let record: Vec<u8> = fixture
            .db
            .read(|c| -> Result<_> {
                Ok(c.query_row(
                    "SELECT record FROM signed_prekeys WHERE identity = 'aci' AND key_id = ?1",
                    [before.signed],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        let record = SignedPreKeyRecord::deserialize(&record).unwrap();
        assert!(fixture.account.aci_identity.public_key().verify_signature(
            &record.public_key().unwrap().serialize(),
            &record.signature().unwrap()
        ));
        assert_eq!(
            record.timestamp().unwrap().epoch_millis(),
            u64::try_from(NOW).unwrap()
        );
    }

    #[test]
    fn a_refill_brings_a_batch_of_each_kind_and_only_that() {
        let fixture = fixture();
        let kind = IdentityKind::Pni;
        let before = fixture.db.read(|c| KeyIds::load(c, kind)).unwrap();
        let refill = Plan {
            one_time: true,
            kyber: true,
            ..Plan::default()
        };
        let prepared = fixture
            .db
            .zone(|tx| prepare(tx, &fixture.account, kind, refill, NOW, &mut rand::rng()))
            .unwrap();
        assert_eq!(prepared.one_time, ONE_TIME_BATCH);
        assert_eq!((prepared.signed_id, prepared.last_resort_id), (None, None));
        assert_eq!(prepared.body.pre_keys.len(), 100);
        assert_eq!(prepared.body.pq_pre_keys.len(), 100);
        assert_eq!(prepared.body.pre_keys[0].key_id, before.prekey);
        assert_eq!(prepared.body.pq_pre_keys[0].key_id, before.kyber);
        let json = serde_json::to_value(&prepared.body).unwrap();
        assert!(json.get("signedPreKey").is_none() && json.get("pqLastResortPreKey").is_none());
        let after = fixture.db.read(|c| KeyIds::load(c, kind)).unwrap();
        assert_eq!(after.prekey, KeyIds::advance(before.prekey, 100));
        assert_eq!(after.kyber, KeyIds::advance(before.kyber, 100));
        assert_eq!(after.signed, before.signed);
        let stored: i64 = fixture
            .db
            .read(|c| -> Result<_> {
                Ok(c.query_row(
                    "SELECT (SELECT count(*) FROM prekeys WHERE identity = 'pni')
                       + (SELECT count(*) FROM kyber_prekeys WHERE identity = 'pni'
                          AND last_resort = 0)",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(stored, 200, "durable before the server can hand them out");
    }

    /// Desktop's `_clean*`: the newest five signed prekeys and last-resort keys stay whatever
    /// their age, older ones go once no queued message can use them (the server's queue time,
    /// at least 30 days); one-time keys beyond the newest 200 go after 90 days.
    #[test]
    fn old_keys_go_once_nothing_can_use_them() {
        let fixture = fixture();
        let kind = IdentityKind::Aci;
        let rng = &mut rand::rng();
        let identity = &fixture.account.aci_identity;
        fixture
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("DELETE FROM signed_prekeys", [])?;
                tx.execute("DELETE FROM kyber_prekeys", [])?;
                // Signed prekeys and last-resort keys made 0, 10, 20, … 70 days ago.
                for n in 0..8u32 {
                    let made = NOW - i64::from(n) * 10 * DAY;
                    let stamp = Timestamp::from_epoch_millis(u64::try_from(made).unwrap());
                    let signed = generate_signed_prekey(identity, 100 + n, stamp, rng)?;
                    insert_signed_prekey(tx, kind, &signed, n != 0)?;
                    let kyber = generate_kyber_prekey(identity, 200 + n)?;
                    insert_kyber_prekey(tx, kind, &kyber, true, true, made)?;
                    tx.execute(
                        "INSERT INTO kyber_triples(identity, kyber_id, signed_id, base_key,
                           used_at) VALUES ('aci', ?1, 1, x'05', 0)",
                        [200 + n],
                    )?;
                }
                // 203 one-time keys: three of them 100 days old, 200 new.
                for n in 0..203u32 {
                    let made = if n < 3 { NOW - 100 * DAY } else { NOW - DAY };
                    let record = PreKeyRecord::new((1000 + n).into(), &KeyPair::generate(rng));
                    insert_prekey(tx, kind, &record, made)?;
                }
                // One-time Kyber keys: 100 days old, but fewer than 200 in all.
                for n in 0..3u32 {
                    let kyber = generate_kyber_prekey(identity, 300 + n)?;
                    insert_kyber_prekey(tx, kind, &kyber, false, false, NOW - 100 * DAY)?;
                }
                Ok(())
            })
            .unwrap();
        let ids = |sql: &str| -> Vec<u32> {
            rows(&fixture.db, sql)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };
        let deleted = fixture
            .db
            .zone(|tx| confirm_and_clean(tx, kind, Some(100), None, NOW))
            .unwrap();
        // Signed: 100..=104 are the newest five; 105 (50 days), 106, 107 are past 30 days.
        assert_eq!(
            ids("SELECT key_id, confirmed FROM signed_prekeys ORDER BY key_id"),
            [100, 101, 102, 103, 104]
        );
        assert_eq!(
            rows(
                &fixture.db,
                "SELECT key_id, confirmed FROM signed_prekeys WHERE key_id = 100"
            ),
            [(100, true)],
            "the uploaded one is confirmed"
        );
        assert_eq!(
            ids("SELECT key_id, 1 FROM kyber_prekeys WHERE last_resort = 1 ORDER BY key_id"),
            [200, 201, 202, 203, 204]
        );
        assert_eq!(
            ids("SELECT kyber_id, 1 FROM kyber_triples ORDER BY kyber_id"),
            [200, 201, 202, 203, 204],
            "the replay guard of a deleted key goes with it"
        );
        assert_eq!(
            ids("SELECT key_id, 1 FROM prekeys ORDER BY key_id").len(),
            200,
            "the three old one-time keys beyond the newest 200"
        );
        assert_eq!(
            ids("SELECT key_id, 1 FROM kyber_prekeys WHERE last_resort = 0").len(),
            3,
            "old, but within the 200 kept"
        );
        assert_eq!(deleted, 3 + 3 + 3);

        // A longer queue time on the server keeps rotated keys longer.
        fixture
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "INSERT INTO remote_config(name, value) VALUES (?1, ?2)",
                    (QUEUE_TIME_KEY, (45 * 24 * 60 * 60).to_string()),
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fixture.db.read(queue_time).unwrap(),
            Duration::from_secs(45 * 24 * 60 * 60)
        );
        fixture
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("UPDATE remote_config SET value = '60'", [])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(fixture.db.read(queue_time).unwrap(), QUEUE_TIME_MIN);
    }

    #[test]
    fn generated_keys_verify_against_the_identity() {
        let rng = &mut rand::rng();
        let identity = IdentityKeyPair::generate(rng);
        let now = Timestamp::from_epoch_millis(1_700_000_000_000);
        let signed = generate_signed_prekey(&identity, 7, now, rng).unwrap();
        assert!(identity.public_key().verify_signature(
            &signed.public_key().unwrap().serialize(),
            &signed.signature().unwrap()
        ));
        let kyber = generate_kyber_prekey(&identity, 9).unwrap();
        assert!(identity.public_key().verify_signature(
            &kyber.public_key().unwrap().serialize(),
            &kyber.signature().unwrap()
        ));
        let json = SignedPreKeyJson::kyber(&kyber).unwrap();
        assert_eq!(json.key_id, 9);
        let text = serde_json::to_string(&json).unwrap();
        assert!(text.contains("\"keyId\":9") && text.contains("\"publicKey\""));
    }

    #[test]
    fn bundles_parse_and_require_kyber() {
        let rng = &mut rand::rng();
        let identity = IdentityKeyPair::generate(rng);
        let now = Timestamp::from_epoch_millis(1);
        let signed = generate_signed_prekey(&identity, 1, now, rng).unwrap();
        let kyber = generate_kyber_prekey(&identity, 2).unwrap();
        let mut device = serde_json::json!({
            "deviceId": 1,
            "registrationId": 4242,
            "signedPreKey": SignedPreKeyJson::signed(&signed).unwrap(),
            "pqPreKey": SignedPreKeyJson::kyber(&kyber).unwrap(),
            "preKey": null,
        });
        let body = |device: &serde_json::Value| {
            serde_json::to_vec(&serde_json::json!({
                "identityKey": STANDARD.encode(identity.identity_key().serialize()),
                "devices": [device],
            }))
            .unwrap()
        };
        let bundles = parse_bundles(&body(&device)).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(u32::from(bundles[0].0), 1);
        assert_eq!(bundles[0].1.registration_id().unwrap(), 4242);

        // The real server writes the two signatures without padding (the mock pads them).
        let mut bare = device.clone();
        for key in ["signedPreKey", "pqPreKey"] {
            let signature = bare[key]["signature"].as_str().unwrap().to_owned();
            assert!(signature.ends_with('='), "a 64-byte signature is padded");
            bare[key]["signature"] = signature.trim_end_matches('=').into();
        }
        let unpadded = parse_bundles(&body(&bare)).unwrap();
        assert_eq!(
            unpadded[0].1.signed_pre_key_signature().unwrap(),
            bundles[0].1.signed_pre_key_signature().unwrap()
        );
        assert_eq!(
            unpadded[0].1.kyber_pre_key_signature().unwrap(),
            bundles[0].1.kyber_pre_key_signature().unwrap()
        );

        device["pqPreKey"] = serde_json::Value::Null;
        assert!(matches!(
            parse_bundles(&body(&device)),
            Err(Error::Decode(_))
        ));
    }
}
