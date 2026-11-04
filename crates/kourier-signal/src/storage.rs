//! Storage Service (§6.9): credentials, the manifest when it is newer than the local version, the
//! missing records, their decryption and the merge of `ContactRecord`, `GroupV2Record` and
//! `AccountRecord` into `backend.db` together with the core events they imply (one zone per ≤
//! [`MERGE_BATCH`] records, §5.6). The writes (A10) live in [`write`] and [`intents`]; the write
//! job fetches at every run and merges through the same path ([`merge_manifest`]).
//!
//! Desktop sources: `SD/ts/services/storage.preload.ts` (`sync`, `processManifest`,
//! `fetchRemoteRecords`; a 404 manifest is created only by a primary, 1514), merge rules from
//! `SD/ts/services/storageRecordOps.preload.ts` (`mergeContactRecord`, `mergeGroupV2Record`,
//! `mergeAccountRecord`), keys from `SD/ts/Crypto.node.ts` (`deriveStorageManifestKey`,
//! `deriveStorageItemKey`).
//!
//! Crash safety: every zone stores its raw records with their events, and only the last zone
//! stores the manifest version. A kill in between leaves the version old, so the next sync
//! reads the manifest again and fetches exactly the records not stored yet: none is lost, and
//! none is merged twice. Records the manifest no longer lists are deleted in the last zone.
//!
//! Conversations: a contact's chat is created or changed only when the record changes its
//! roaming state (archived, marked unread, muted, blocked), when it is pinned, or when the core
//! already has the chat and its accepted state changed; contacts without either stay contacts,
//! so the chat list does not fill with empty chats. Groups are stored (master key and flags);
//! a group never fetched gets a `group_refresh` job and joins the core with its state (S10).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit as _, Mac as _};
use http::{HeaderValue, Method, StatusCode};
use kourier_backend_api::BackendEvent;
use kourier_db::Tx;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{
    AccountSettingsPatch, ContactPatch, ConvKey, ConversationPatch, Identifier, IdentifierKind,
    Patch, UserId,
};
use libsignal_core::{Aci, Pni, ServiceId};
use libsignal_protocol::IdentityKey;
use prost::Message as _;
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::Notify;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::actor::Shared;
use crate::crypto::decrypt_gcm;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::jobs::{self, Done, JobKind};
use crate::net::{ChatRequest, HttpsHost, HttpsRequest, SignalTransport, parse_json};
use crate::outbox::append_events;
use crate::peers::Roaming;
use crate::proto::{self, manifest_record::identifier::Type as ItemType, storage_record::Record};
use crate::service_id::{aci_user_id, service_id_from_wire, service_id_user_id, validate_e164};
use crate::{db, features, peers};

/// After a merge zone committed, before the next one (the kill test of §10.4).
pub(crate) const FAILPOINT_AFTER_STORAGE_MERGE_COMMIT: &str = "after_storage_merge_commit";

/// Storage credentials on the authenticated chat socket.
const AUTH_PATH: &str = "/v1/storage/auth";
/// The newest manifest.
const MANIFEST_PATH: &str = "/v1/storage/manifest";
/// Reading records.
const READ_PATH: &str = "/v1/storage/read";
/// Most record keys per read request.
const READ_BATCH: usize = 1000;
/// Most records merged in one zone (§6.9).
pub(crate) const MERGE_BATCH: usize = 500;
/// HKDF info prefix of record keys derived from the manifest's `recordIkm`.
const ITEM_INFO_PREFIX: &[u8] = b"20240801_SIGNAL_STORAGE_SERVICE_ITEM_";
/// Timeout of one Storage Service request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// Keys and decryption
// ---------------------------------------------------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    // HMAC takes keys of any length.
    if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) {
        mac.update(data);
        out.copy_from_slice(&mac.finalize().into_bytes());
    }
    out
}

/// The key of manifest `version`: `HMAC-SHA256(storageKey, "Manifest_<version>")`.
pub(crate) fn manifest_key(storage_key: &[u8; 32], version: u64) -> Zeroizing<[u8; 32]> {
    hmac_sha256(storage_key, format!("Manifest_{version}").as_bytes())
}

/// The key of record `raw_id`: HKDF-SHA256 over the manifest's `recordIkm` with info
/// `20240801_SIGNAL_STORAGE_SERVICE_ITEM_ ‖ raw id`, or, for manifests without one, the legacy
/// `HMAC-SHA256(storageKey, "Item_" ‖ base64(raw id))`.
pub(crate) fn item_key(
    storage_key: &[u8; 32],
    record_ikm: Option<&[u8]>,
    raw_id: &[u8],
) -> Zeroizing<[u8; 32]> {
    match record_ikm.filter(|ikm| !ikm.is_empty()) {
        Some(ikm) => {
            let mut info = Vec::with_capacity(ITEM_INFO_PREFIX.len() + raw_id.len());
            info.extend_from_slice(ITEM_INFO_PREFIX);
            info.extend_from_slice(raw_id);
            let mut key = Zeroizing::new([0u8; 32]);
            // 32 bytes is far below HKDF-SHA256's limit.
            let _ = Hkdf::<Sha256>::new(None, ikm).expand(&info, key.as_mut());
            key
        }
        None => hmac_sha256(
            storage_key,
            format!("Item_{}", STANDARD.encode(raw_id)).as_bytes(),
        ),
    }
}

/// Decrypts and decodes manifest `version`.
pub(crate) fn decrypt_manifest(
    storage_key: &[u8; 32],
    version: u64,
    value: &[u8],
) -> Result<proto::ManifestRecord> {
    let plain = decrypt_gcm(&manifest_key(storage_key, version), value)
        .map_err(|_| Error::Decode("storage manifest (wrong storage key?)"))?;
    let record =
        proto::ManifestRecord::decode(&plain[..]).map_err(|_| Error::Decode("storage manifest"))?;
    if record.version != version {
        return Err(Error::Decode("storage manifest version"));
    }
    Ok(record)
}

/// Decrypts and decodes one record.
pub(crate) fn decrypt_item(
    storage_key: &[u8; 32],
    record_ikm: Option<&[u8]>,
    raw_id: &[u8],
    value: &[u8],
) -> Result<(proto::StorageRecord, Zeroizing<Vec<u8>>)> {
    let plain = decrypt_gcm(&item_key(storage_key, record_ikm, raw_id), value)
        .map_err(|_| Error::Decode("storage record"))?;
    let record =
        proto::StorageRecord::decode(&plain[..]).map_err(|_| Error::Decode("storage record"))?;
    Ok((record, plain))
}

// ---------------------------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthBody {
    username: String,
    password: String,
}

/// `Authorization: Basic …` for the storage host.
struct Credentials(HeaderValue);

/// `GET /v1/storage/auth` on the authenticated socket.
async fn credentials<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
) -> Result<Credentials> {
    let response = shared
        .net
        .request(socket, ChatRequest::get(AUTH_PATH)?)
        .await?;
    if !response.status.is_success() {
        return Err(Error::Status {
            what: "storage credentials",
            status: response.status.as_u16(),
        });
    }
    let body: AuthBody = parse_json(&response)?;
    let password = Zeroizing::new(body.password);
    let token = Zeroizing::new(STANDARD.encode(format!("{}:{}", body.username, *password)));
    let mut value = HeaderValue::from_str(&format!("Basic {}", token.as_str()))
        .map_err(|_| Error::Decode("storage credentials"))?;
    value.set_sensitive(true);
    Ok(Credentials(value))
}

/// What `GET /v1/storage/manifest[/version/<n>]` answered.
enum Fetched {
    /// 204: nothing newer than the local version.
    Unchanged,
    /// 404: the account has no manifest (a linked device never creates one).
    Missing,
    /// 200.
    Manifest(proto::StorageManifest),
}

async fn fetch_manifest<T: SignalTransport>(
    shared: &Shared<T>,
    credentials: &Credentials,
    local: Option<u64>,
) -> Result<Fetched> {
    let path = match local {
        Some(version) => format!("{MANIFEST_PATH}/version/{version}"),
        None => MANIFEST_PATH.to_owned(),
    };
    let mut request = HttpsRequest::get(HttpsHost::Storage, path);
    request.timeout = REQUEST_TIMEOUT;
    request
        .headers
        .insert(http::header::AUTHORIZATION, credentials.0.clone());
    let response = shared.net.https(request).await?;
    match response.status {
        StatusCode::NO_CONTENT => Ok(Fetched::Unchanged),
        StatusCode::NOT_FOUND => Ok(Fetched::Missing),
        status if status.is_success() => proto::StorageManifest::decode(&response.body[..])
            .map(Fetched::Manifest)
            .map_err(|_| Error::Decode("storage manifest")),
        status => Err(Error::Status {
            what: "storage manifest",
            status: status.as_u16(),
        }),
    }
}

/// `PUT /v1/storage/read` in batches of [`READ_BATCH`]; returns the items by key.
async fn read_items<T: SignalTransport>(
    shared: &Shared<T>,
    credentials: &Credentials,
    keys: &[Vec<u8>],
) -> Result<HashMap<Vec<u8>, Vec<u8>>> {
    let mut items = HashMap::with_capacity(keys.len());
    for batch in keys.chunks(READ_BATCH) {
        let body = proto::ReadOperation {
            read_key: batch.to_vec(),
        }
        .encode_to_vec();
        let mut request = HttpsRequest::new(Method::PUT, HttpsHost::Storage, READ_PATH);
        request.timeout = REQUEST_TIMEOUT;
        request.body = Some(body.into());
        request
            .headers
            .insert(http::header::AUTHORIZATION, credentials.0.clone());
        request.headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-protobuf"),
        );
        let response = shared.net.https(request).await?;
        if !response.status.is_success() {
            return Err(Error::Status {
                what: "storage read",
                status: response.status.as_u16(),
            });
        }
        let decoded = proto::StorageItems::decode(&response.body[..])
            .map_err(|_| Error::Decode("storage items"))?;
        for item in decoded.items {
            items.insert(item.key, item.value);
        }
    }
    Ok(items)
}

// ---------------------------------------------------------------------------------------------
// The sync job
// ---------------------------------------------------------------------------------------------

/// The storage state of the account row.
struct LocalState {
    storage_key: Zeroizing<[u8; 32]>,
    version: Option<u64>,
}

fn local_state(conn: &Connection) -> Result<LocalState> {
    let (key, version): (Zeroizing<Vec<u8>>, Option<i64>) = conn.query_row(
        "SELECT storage_key, manifest_version FROM account WHERE id = 1",
        [],
        |r| Ok((Zeroizing::new(r.get(0)?), r.get(1)?)),
    )?;
    let storage_key = Zeroizing::new(
        <[u8; 32]>::try_from(key.as_slice()).map_err(|_| Error::Decode("storage key"))?,
    );
    Ok(LocalState {
        storage_key,
        version: version.and_then(|v| u64::try_from(v).ok()),
    })
}

/// One decrypted record waiting for its zone.
struct Decrypted {
    key: Vec<u8>,
    kind: i32,
    record: proto::StorageRecord,
    raw: Zeroizing<Vec<u8>>,
}

/// Merge order: contacts first, the account record (pins, which name contacts) last.
fn merge_rank(kind: i32) -> u8 {
    match ItemType::try_from(kind) {
        Ok(ItemType::Contact) => 0,
        Ok(ItemType::Groupv2) => 1,
        Ok(ItemType::Account) => 3,
        _ => 2,
    }
}

/// Records "the sync finished" (nothing to merge).
fn mark_synced(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE account SET storage_synced_at = ?1 WHERE id = 1",
        [now],
    )?;
    Ok(())
}

/// Runs one Storage Service sync (the `storage_sync` job): fetches the manifest when it is newer
/// than the local version and merges it ([`merge_manifest`]).
pub(crate) async fn sync<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let local = shared.db.read(local_state)?;
    let credentials = credentials(shared, socket).await?;
    let manifest = match fetch_manifest(shared, &credentials, local.version).await? {
        Fetched::Unchanged => {
            let now = shared.now();
            shared.db.zone(|tx| mark_synced(tx, now))?;
            tracing::info!("storage service: manifest unchanged");
            return Ok(Done::Finished);
        }
        Fetched::Missing => {
            let now = shared.now();
            shared.db.zone(|tx| mark_synced(tx, now))?;
            tracing::info!("storage service: no manifest; nothing to merge");
            return Ok(Done::Finished);
        }
        Fetched::Manifest(manifest) => manifest,
    };
    merge_manifest(shared, &account, &local, &credentials, &manifest).await?;
    Ok(Done::Finished)
}

/// THE MERGE PATH of a fetched manifest, the one both `storage_sync` and the write job
/// (`write::run`, which fetches at every run, BQ7) go through: it decrypts the manifest, judges a
/// write of ours in flight by it and, when it is newer than the local version, reads the records
/// not stored yet and merges them. Returns the decrypted manifest (the write job plans on it).
///
/// A write in flight is judged in two places on purpose. One that did NOT land is resolved right
/// after the manifest decrypts, before the items are read (review N1 (a)). One that landed is
/// landed inside the first merge zone (review B1), and from there to the commit of the last zone
/// nothing awaits, so no command runs between the landing and the merges of that version. A row
/// the manifest cannot judge here (a manifest behind the row, or one that is not newer than the
/// local version, so no merge zone runs) stays for the caller: the sync leaves it to the write
/// job, the job judges it (`write::run`).
async fn merge_manifest<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    account: &Account,
    local: &LocalState,
    credentials: &Credentials,
    manifest: &proto::StorageManifest,
) -> Result<proto::ManifestRecord> {
    let version = manifest.version;
    let record = match decrypt_manifest(&local.storage_key, version, &manifest.value) {
        Ok(record) => record,
        Err(e) => {
            // Our storage key is stale (the phone rotated its keys and we missed the KEYS
            // sync): ask the phone again, as Desktop's `stopStorageServiceSync` does; the
            // answer resets the Storage Service and queues a full sync (`apply_keys`).
            let now = shared.now();
            let asked = shared
                .db
                .zone(|tx| crate::maintenance::request_keys(tx, now))?;
            if asked {
                shared.jobs_wake.notify_one();
            }
            tracing::warn!(version, asked, "storage manifest did not decrypt");
            return Err(e);
        }
    };
    // A write of ours in flight is judged by THIS manifest at once (review N1 (a)): one that did
    // not land (or a row this build cannot read) is resolved in a zone of its own BEFORE the items
    // are read, so a click, a reading or an unpin made while they are read finds no row and takes
    // the phone's value as its base. Only a landed write waits for the first merge zone (review
    // B1, below). With no row in flight no zone is opened.
    if shared.db.read(write::in_flight_exists)? {
        let now = shared.now();
        shared
            .db
            .zone(|tx| intents::resolve_not_landed(tx, account, &record, now, &shared.wake))?;
    }
    if local.version.is_some_and(|local| version <= local) {
        let now = shared.now();
        shared.db.zone(|tx| mark_synced(tx, now))?;
        return Ok(record);
    }
    let stored: BTreeSet<Vec<u8>> = shared.db.read(stored_keys)?;
    let mut wanted: Vec<(Vec<u8>, i32)> = Vec::with_capacity(record.identifiers.len());
    let mut seen = BTreeSet::new();
    for identifier in &record.identifiers {
        if !identifier.raw.is_empty() && seen.insert(identifier.raw.clone()) {
            wanted.push((identifier.raw.clone(), identifier.r#type));
        }
    }
    let removed: Vec<Vec<u8>> = stored
        .iter()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    let missing: Vec<(Vec<u8>, i32)> = wanted
        .into_iter()
        .filter(|(key, _)| !stored.contains(key))
        .collect();
    let keys: Vec<Vec<u8>> = missing.iter().map(|(key, _)| key.clone()).collect();
    let mut items = read_items(shared, credentials, &keys).await?;
    let record_ikm = Some(record.record_ikm.as_slice()).filter(|ikm| !ikm.is_empty());
    let mut decrypted = Vec::with_capacity(missing.len());
    let mut failed = 0usize;
    for (key, kind) in missing {
        let Some(value) = items.remove(&key) else {
            failed += 1;
            continue;
        };
        match decrypt_item(&local.storage_key, record_ikm, &key, &value) {
            Ok((record, raw)) => decrypted.push(Decrypted {
                key,
                kind,
                record,
                raw,
            }),
            Err(_) => failed += 1,
        }
    }
    decrypted.sort_by_key(|d| merge_rank(d.kind));
    tracing::info!(
        version,
        new = decrypted.len(),
        removed = removed.len(),
        failed,
        "storage service: merging"
    );
    let first_sync = local.version.is_none();
    let zones = decrypted.len().div_ceil(MERGE_BATCH).max(1);
    let mut chunks = decrypted.chunks(MERGE_BATCH);
    let mut outcome = ZoneOutcome::default();
    // The new identifiers of the write the first zone lands: OUR records (review N6, L1).
    let mut ours: HashSet<Vec<u8>> = HashSet::new();
    // From here to the commit of the last zone nothing awaits: the zones are synchronous and run
    // back to back, so no command runs between the landing below and the merges of this version.
    for index in 0..zones {
        let batch = chunks.next().unwrap_or(&[]);
        let last = (index + 1 == zones).then_some(LastZone {
            version,
            removed: &removed,
        });
        let now = shared.now();
        let zone = shared.db.zone(|tx| {
            // A write of ours that was in flight when the app stopped is judged by THIS manifest
            // in the first merge zone, before its merge (critic X4, review B1): the records it
            // lists would otherwise be merged as the phone's own, against intents whose base has
            // not moved yet. It runs AFTER the items were read, inside the transaction of the
            // merge: a click, a reading or an unpin made while they were read still found the row
            // (and took the written value as its base), and none can come between the landing and
            // the merge of our own records. With no row this reads one table.
            if index == 0 {
                let keys = write::read_in_flight(tx)?.map(|row| row.new_keys);
                if let intents::Resolved::Found { landing, .. } =
                    intents::resolve_in_flight(tx, account, &record, now, &shared.wake)?
                {
                    ours.extend(keys.into_iter().flatten());
                    write::after_landing(tx, landing, now)?;
                }
            }
            merge_zone_with(
                tx,
                account,
                batch,
                last.as_ref(),
                Origin {
                    first_sync,
                    ours: &ours,
                },
                now,
                &shared.wake,
            )
        })?;
        kourier_db::failpoint::hit(FAILPOINT_AFTER_STORAGE_MERGE_COMMIT);
        outcome.profile_fetches |= zone.profile_fetches;
        outcome.own_profile_key |= zone.own_profile_key;
        outcome.group_refreshes |= zone.group_refreshes;
        outcome.released |= zone.released;
    }
    if outcome.own_profile_key
        && let Some(reloaded) = shared.db.read(Account::load)?
    {
        *shared.account.borrow_mut() = Some(reloaded);
    }
    if outcome.profile_fetches || outcome.group_refreshes || outcome.released {
        shared.jobs_wake.notify_one();
    }
    Ok(record)
}

fn stored_keys(conn: &Connection) -> Result<BTreeSet<Vec<u8>>> {
    let mut stmt = conn.prepare_cached("SELECT key FROM storage_records")?;
    let keys = stmt
        .query_map([], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(keys)
}

/// What only the last zone of a sync does.
struct LastZone<'a> {
    version: u64,
    removed: &'a [Vec<u8>],
}

/// What a zone did that the job acts on after the commit.
#[derive(Default)]
struct ZoneOutcome {
    /// Profile fetches were queued.
    profile_fetches: bool,
    /// Our own profile key changed.
    own_profile_key: bool,
    /// Group refresh jobs were queued.
    group_refreshes: bool,
    /// A contact's key left state 2: the jobs waiting on it are due (P5 M1).
    released: bool,
}

/// One merge zone: the batch's records into `backend.db` and their events into `core_outbox`;
/// none of them is ours ([`merge_zone_with`]). The tests' merge (the sync names our own records).
#[cfg(test)]
fn merge_zone(
    tx: &mut Tx<'_>,
    account: &Account,
    batch: &[Decrypted],
    last: Option<&LastZone<'_>>,
    first_sync: bool,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<ZoneOutcome> {
    let none = HashSet::new();
    merge_zone_with(
        tx,
        account,
        batch,
        last,
        Origin {
            first_sync,
            ours: &none,
        },
        now,
        wake,
    )
}

/// Where the records of a merge zone come from.
struct Origin<'a> {
    /// No manifest was merged before: the Storage Service's profile keys win over local ones.
    first_sync: bool,
    /// The identifiers of the write the sync just landed (review N6, L1): those records are OURS.
    ours: &'a HashSet<Vec<u8>>,
}

/// [`merge_zone`] with OUR records named: each is stored with `ours = 1` (the automatic clear's
/// N-C does not count it as the phone's newer mark) and states no flag of its chat (the landing
/// right before it stated what it should: with the switch off the written values but an archive
/// a message overtook, review L1; with the switch on the intents' rule runs as for any record).
fn merge_zone_with(
    tx: &mut Tx<'_>,
    account: &Account,
    batch: &[Decrypted],
    last: Option<&LastZone<'_>>,
    origin: Origin<'_>,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<ZoneOutcome> {
    let mut merge = Merge {
        account,
        now,
        first_sync: origin.first_sync,
        intents: intents::any_intent(tx)?,
        ours: false,
        events: Vec::new(),
        outcome: ZoneOutcome::default(),
    };
    for item in batch {
        merge.ours = origin.ours.contains(&item.key);
        let peer = match &item.record.record {
            Some(Record::Contact(contact)) => merge_contact(tx, &mut merge, &item.key, contact)?,
            Some(Record::GroupV2(group)) => {
                merge_group(tx, &mut merge, group)?;
                None
            }
            Some(Record::Account(record)) => {
                merge_account(tx, &mut merge, &item.key, record)?;
                None
            }
            _ => None,
        };
        // What the record belongs to, for the write engine (A10): decided from the record itself,
        // by the one function `fill_targets` uses for a row from before migration 0025.
        let target = target_of(account, &item.record);
        tx.prepare_cached(
            "INSERT INTO storage_records(key, kind, peer, target, record, updated_at, ours)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(key) DO UPDATE SET kind = excluded.kind, peer = excluded.peer,
               target = excluded.target, record = excluded.record,
               updated_at = excluded.updated_at, ours = excluded.ours",
        )?
        .execute((
            &item.key,
            item.kind,
            peer,
            target,
            item.raw.as_slice(),
            now,
            merge.ours,
        ))?;
    }
    if let Some(last) = last {
        for key in last.removed {
            tx.prepare_cached("DELETE FROM storage_records WHERE key = ?1")?
                .execute([key])?;
        }
        tx.execute(
            "UPDATE account SET manifest_version = ?1, storage_synced_at = ?2 WHERE id = 1",
            (i64::try_from(last.version).unwrap_or(i64::MAX), now),
        )?;
    }
    let events = features::with_features(tx, account, merge.events, now)?;
    if !events.is_empty() {
        append_events(tx, &events, now, wake)?;
    }
    Ok(merge.outcome)
}

/// The state of one merge zone.
struct Merge<'a> {
    account: &'a Account,
    now: i64,
    /// No manifest was merged before: the Storage Service's profile keys win over local ones.
    first_sync: bool,
    /// An intent of the write engine is stored (A10): only then do the merges run the three-way
    /// rule (`intents::three_way`); with none every merge is what it was before there were
    /// intents.
    intents: bool,
    /// The record being merged is OURS (review N6, L1): the write the sync just landed put it
    /// there. Its flags are not diffed (see [`Merge::settle`]).
    ours: bool,
    events: Vec<BackendEvent>,
    outcome: ZoneOutcome,
}

impl Merge<'_> {
    /// The three-way rule for the archive, mark and mute intents of `target` against the flags
    /// of the record being merged (§3.3); nothing is settled while no intent is stored. OUR own
    /// record (review N6, L1) runs no flag diff: a field no intent holds is held at `told` (what
    /// the core was last told: nothing is stated, the mirror stays), because the landing right
    /// before this merge already stated what it should (with the switch off the written values,
    /// but not an `archived = 1` a message overtook; with the switch on the core shows ours).
    fn settle(
        &self,
        conn: &Connection,
        target: &str,
        theirs: intents::Flags,
        told: intents::Flags,
    ) -> Result<intents::Settlement> {
        let settlement = if self.intents {
            intents::settle_flags(conn, target, theirs)?
        } else {
            intents::Settlement::default()
        };
        let our_own_record = self.ours;
        Ok(if our_own_record {
            settlement.quiet(told)
        } else {
            settlement
        })
    }
}

/// The stored record of `peer` other than `key` (the previous version of a changed record);
/// `None` when there is none.
fn previous_contact(
    conn: &Connection,
    peer: &str,
    key: &[u8],
) -> Result<Option<proto::ContactRecord>> {
    stored_contact(conn, peer, Some(key))
}

/// The newest stored record of `peer`, other than `except` when that is given.
fn stored_contact(
    conn: &Connection,
    peer: &str,
    except: Option<&[u8]>,
) -> Result<Option<proto::ContactRecord>> {
    let raw: Option<Vec<u8>> = conn
        .prepare_cached(
            "SELECT record FROM storage_records WHERE peer = ?1 AND (?2 IS NULL OR key != ?2)
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
        )?
        .query_row((peer, except), |r| r.get(0))
        .optional()?;
    Ok(raw
        .and_then(|raw| proto::StorageRecord::decode(&raw[..]).ok())
        .and_then(|record| match record.record {
            Some(Record::Contact(contact)) => Some(contact),
            _ => None,
        }))
}

/// Whether address-book photos win over profile pictures (`AccountRecord.preferContactAvatars`,
/// Desktop's `getAvatar` in `SD/ts/util/avatarUtils.preload.ts`): the phone's setting, from the
/// newest account record stored. `false` (profile pictures first) until one arrived.
pub(crate) fn prefer_contact_avatars(conn: &Connection) -> Result<bool> {
    let raw: Option<Vec<u8>> = conn
        .prepare_cached(
            "SELECT record FROM storage_records WHERE kind = ?1 ORDER BY updated_at DESC, rowid DESC
             LIMIT 1",
        )?
        .query_row([ItemType::Account as i32], |r| r.get(0))
        .optional()?;
    Ok(raw
        .and_then(|raw| proto::StorageRecord::decode(&raw[..]).ok())
        .is_some_and(|record| match record.record {
            Some(Record::Account(account)) => account.prefer_contact_avatars,
            _ => false,
        }))
}

/// Signal's "Keep muted chats archived" (`AccountRecord.keepMutedChatsArchived`, field 25), as a
/// column the receive path reads with a row it has already. Every account record merged writes
/// it (`merge_account`). NULL (a profile from before migration 0023, whose newest account
/// record was stored long ago and is never merged again until the phone changes an account
/// setting) is filled ONCE here, offline, from the newest stored account record: `false` when
/// there is none (Desktop's `?? false`). Called in a write zone, like [`fill_roaming`].
pub(crate) fn keep_muted_chats_archived(conn: &Connection) -> Result<bool> {
    let told: Option<Option<bool>> = conn
        .prepare_cached("SELECT keep_muted_archived FROM account WHERE id = 1")?
        .query_row([], |r| r.get(0))
        .optional()?;
    match told {
        Some(Some(keep)) => Ok(keep),
        Some(None) => {
            let keep = stored_account(conn, None)?.is_some_and(|r| r.keep_muted_chats_archived);
            conn.prepare_cached("UPDATE account SET keep_muted_archived = ?1 WHERE id = 1")?
                .execute([keep])?;
            Ok(keep)
        }
        None => Ok(false),
    }
}

/// The archive state and mute the phone last stated for the 1:1 chat of `sid` (Note to Self
/// when `sid` is our own ACI), for a peer whose `peers.roaming_*` base was never written
/// (A1, the lead's BQ1 (a): a profile from before migration 0023, or a chat no record has
/// stated): read ONCE from the newest stored record, offline, written as the base and returned.
/// No record at all means the phone says nothing of the chat: not archived, not muted, and
/// that is the base too, so the decode is never repeated for the peer. A later record is
/// diffed against the base (`merge_contact`). The flag returned beside the base is whether a
/// stored record gave it: only then is it the phone's word the core may not have heard (the
/// receive path states it, `receive/unarchive.rs`).
pub(crate) fn fill_roaming(
    conn: &Connection,
    account: &Account,
    sid: &str,
    now: i64,
) -> Result<(Roaming, bool)> {
    let own = ServiceId::from(account.aci).service_id_string() == sid;
    let from_record = if own {
        stored_account(conn, None)?.map(|record| Roaming {
            archived: record.note_to_self_archived,
            muted_until: None,
        })
    } else {
        stored_contact(conn, sid, None)?.map(|record| Roaming {
            archived: record.archived,
            muted_until: muted_until(record.muted_until_timestamp),
        })
    };
    let recorded = from_record.is_some();
    let roaming = from_record.unwrap_or_default();
    peers::set_roaming(conn, sid, roaming, now)?;
    Ok((roaming, recorded))
}

// ---------------------------------------------------------------------------------------------
// Targets (A10: what a stored record belongs to)
// ---------------------------------------------------------------------------------------------

/// `storage_records.target` of the account record: its pins and the Note to Self flags.
pub(crate) const ACCOUNT_TARGET: &str = "account";

/// `storage_records.target` of a record nobody can ask to change: a kind the write engine never
/// edits (stories, chat folders, call links, ...), a contact record that names nobody Kourier can
/// address or only ourselves, a group record without a 32-byte master key. It is the empty string
/// and not NULL: NULL means "not filled yet" (a row from before migration 0025), and a filled row
/// must not be decoded again by [`fill_targets`].
pub(crate) const NO_TARGET: &str = "";

/// The target of the contact record of the person with service id string `sid`.
pub(crate) fn contact_target(sid: &str) -> String {
    format!("contact:{sid}")
}

/// The service id string a [`contact_target`] names (`None` for any other target).
pub(crate) fn contact_sid(target: &str) -> Option<&str> {
    target.strip_prefix("contact:")
}

/// The target of the group record with this master key: lower-case hex, so no key is in a log by
/// accident (a target is never logged; only counts are).
pub(crate) fn group_target(master: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut target = String::with_capacity("group:".len() + master.len() * 2);
    target.push_str("group:");
    for byte in master {
        let _ = write!(target, "{byte:02x}");
    }
    target
}

/// What a decoded record belongs to: `contact:<sid>`, `group:<master key hex>`, `account`, or
/// [`NO_TARGET`]. Pure; the merge zone stores it with every record and [`fill_targets`] computes
/// it for the rows that were stored before there was a target column.
pub(crate) fn target_of(account: &Account, record: &proto::StorageRecord) -> String {
    match &record.record {
        Some(Record::Contact(contact)) => contact_ids(account, contact)
            .map_or_else(|| NO_TARGET.to_owned(), |ids| contact_target(&ids.sid)),
        Some(Record::GroupV2(group)) if group.master_key.len() == 32 => {
            group_target(&group.master_key)
        }
        Some(Record::Account(_)) => ACCOUNT_TARGET.to_owned(),
        _ => NO_TARGET.to_owned(),
    }
}

/// Gives every stored record that has no target yet (`target IS NULL`: a row stored before
/// migration 0025) its [`target_of`], decoding each such row ONCE, offline: a filled row is never
/// read again, one that does not decode stays NULL (nothing is guessed). Returns how many rows
/// were filled. Called in a write zone by whoever needs the records by target (the intents, the
/// write job); with no intent and no write nothing calls it, so a profile that never turns the
/// sync on never pays for it.
pub(crate) fn fill_targets(conn: &Connection, account: &Account) -> Result<usize> {
    let rows: Vec<(Vec<u8>, Vec<u8>)> = conn
        .prepare_cached("SELECT key, record FROM storage_records WHERE target IS NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut update =
        conn.prepare_cached("UPDATE storage_records SET target = ?2 WHERE key = ?1")?;
    let mut filled = 0;
    for (key, raw) in rows {
        let Ok(record) = proto::StorageRecord::decode(&raw[..]) else {
            continue;
        };
        update.execute((&key, target_of(account, &record)))?;
        filled += 1;
    }
    Ok(filled)
}

/// The stored account record other than `key`; `None` when there is none.
fn previous_account(conn: &Connection, key: &[u8]) -> Result<Option<proto::AccountRecord>> {
    stored_account(conn, Some(key))
}

/// The newest stored account record, other than `except` when that is given.
fn stored_account(
    conn: &Connection,
    except: Option<&[u8]>,
) -> Result<Option<proto::AccountRecord>> {
    let raw: Option<Vec<u8>> = conn
        .prepare_cached(
            "SELECT record FROM storage_records WHERE kind = ?1 AND (?2 IS NULL OR key != ?2)
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
        )?
        .query_row((ItemType::Account as i32, except), |r| r.get(0))
        .optional()?;
    Ok(raw
        .and_then(|raw| proto::StorageRecord::decode(&raw[..]).ok())
        .and_then(|record| match record.record {
            Some(Record::Account(account)) => Some(account),
            _ => None,
        }))
}

/// A UUID from its binary (16 bytes, preferred) or string form; `PNI:` prefixes are accepted.
fn uuid_field(binary: &[u8], string: &str) -> Option<Uuid> {
    if binary.len() == 16 {
        return Uuid::from_slice(binary).ok();
    }
    let string = string.strip_prefix("PNI:").unwrap_or(string);
    (!string.is_empty())
        .then(|| Uuid::try_parse(string).ok())
        .flatten()
}

/// "Given Family" from two optional parts.
fn join_name(given: &str, family: &str) -> Option<String> {
    let given = given.trim();
    let family = family.trim();
    match (given.is_empty(), family.is_empty()) {
        (true, true) => None,
        (false, true) => Some(given.to_owned()),
        (true, false) => Some(family.to_owned()),
        (false, false) => Some(format!("{given} {family}")),
    }
}

/// The address-book name of a contact record.
fn system_name(record: &proto::ContactRecord) -> Option<String> {
    join_name(&record.system_given_name, &record.system_family_name)
        .or_else(|| Some(record.system_nickname.trim().to_owned()).filter(|n| !n.is_empty()))
}

/// The nickname we gave a contact.
fn nickname(record: &proto::ContactRecord) -> Option<String> {
    record
        .nickname
        .as_ref()
        .and_then(|n| join_name(&n.given, &n.family))
}

/// A diffed optional field: `Set` when it appeared or changed, `Clear` when it went away.
fn diff_patch<T: PartialEq + Clone>(new: Option<T>, old: Option<T>) -> Patch<T> {
    match (new, old) {
        (Some(new), Some(old)) if new == old => Patch::Unchanged,
        (Some(new), _) => Patch::Set(new),
        (None, Some(_)) => Patch::Clear,
        (None, None) => Patch::Unchanged,
    }
}

/// `mutedUntilTimestamp` as a column value: 0 is none, `i64::MAX` and more is for ever. The
/// history import converts a chat frame's `muteUntilMs` through this too (on the `u64`,
/// before any cast), so that the sync that follows the import sees no change that is none.
pub(crate) fn muted_until(ts: u64) -> Option<i64> {
    (ts > 0).then(|| i64::try_from(ts).unwrap_or(i64::MAX))
}

/// Who a `ContactRecord` is about.
struct ContactIds {
    aci: Option<Aci>,
    pni: Option<Pni>,
    /// The service id string of the peer rows and of the target: the ACI, else the PNI.
    sid: String,
    user: UserId,
}

/// The person a `ContactRecord` names; `None` for a record that names nobody Kourier can address
/// (no id, an id that is no Signal user) or only ourselves. The ONE place that decides it: the merge
/// and [`target_of`] both read it, so a record's `peer` and its target never disagree.
fn contact_ids(account: &Account, record: &proto::ContactRecord) -> Option<ContactIds> {
    let aci = uuid_field(&record.aci_binary, &record.aci).map(Aci::from);
    let pni = uuid_field(&record.pni_binary, &record.pni).map(Pni::from);
    let service_id: ServiceId = match (aci, pni) {
        (Some(aci), _) => aci.into(),
        (None, Some(pni)) => pni.into(),
        (None, None) => return None,
    };
    if account.kind_of(service_id).is_some()
        || aci.is_some_and(|aci| aci == account.aci)
        || pni.is_some_and(|pni| pni == account.pni)
    {
        return None;
    }
    let user = service_id_user_id(service_id).ok()?;
    Some(ContactIds {
        aci,
        pni,
        sid: service_id.service_id_string(),
        user,
    })
}

/// `ContactRecord` → peers, identities and the contact's core events.
fn merge_contact(
    tx: &Connection,
    merge: &mut Merge<'_>,
    key: &[u8],
    record: &proto::ContactRecord,
) -> Result<Option<String>> {
    let now = merge.now;
    let Some(ContactIds {
        aci,
        pni,
        sid,
        user,
    }) = contact_ids(merge.account, record)
    else {
        return Ok(None);
    };
    let e164 = Some(record.e164.as_str()).filter(|n| validate_e164(n).is_ok());
    let prev = previous_contact(tx, &sid, key)?;
    let first_record = prev.is_none();
    let prev = prev.unwrap_or_default();
    let prev_e164 = Some(prev.e164.as_str()).filter(|n| !n.is_empty());

    // Records that join ACI, PNI and number (Desktop's `maybeMergeContacts`, §3.3).
    if let Some(aci) = aci {
        // The pair goes through the one place that remembers it (`links`): told once,
        // whichever source had it first.
        if let Some(pni) = pni
            && let Some(event) = crate::links::learn(tx, merge.account, pni, aci, now)?
        {
            merge.events.push(event);
        }
        // The number's pair too, told once (`number_links`): a record that comes back (removed
        // and added, or every record after `reset`) must not make the core merge it again. A
        // record whose number the previous one already stated was told before the pairs were
        // remembered (backend.db before schema 21): it is recorded and not told again.
        if let Some(number) = e164
            && let Some(event) = crate::links::learn_number(tx, merge.account, number, aci, now)?
            && prev_e164 != Some(number)
        {
            merge.events.push(event);
        }
    }

    // A contact our phone knows only by number: the number is kept with the PNI, so that the
    // ACI a sealed sender's certificate states for it can be joined (`links::number_of_aci`).
    if aci.is_none()
        && let (Some(pni), Some(number)) = (pni, e164)
    {
        crate::links::number_of_pni(tx, pni, number, now)?;
    }

    let before = peers::load(tx, &sid)?;
    let mut patch = ContactPatch::new(user.clone());

    // Profile key: the Storage Service wins on the first sync or when we have none.
    if let Some(aci) = aci
        && record.profile_key.len() == 32
        && (merge.first_sync || peers::profile_key(tx, &sid)?.is_none())
        && peers::store_profile_key(tx, aci, &record.profile_key, now)?
    {
        jobs::request(tx, JobKind::ProfileFetch, &sid, now, None)?;
        merge.outcome.profile_fetches = true;
    }
    // Profile name until the profile itself was fetched.
    if aci.is_some()
        && let Some(name) = join_name(&record.given_name, &record.family_name)
        && !profile_fetched(tx, &sid)?
    {
        patch.profile_name = Patch::Set(name);
    }
    let system = system_name(record);
    patch.system_name = diff_patch(system.clone(), system_name(&prev));
    patch.nickname = diff_patch(nickname(record), nickname(&prev));
    patch.blocked = Some(record.blocked);
    patch.hidden = Some(record.hidden);
    if record.unregistered_at_timestamp > 0 {
        patch.unregistered_at =
            Patch::Set(i64::try_from(record.unregistered_at_timestamp).unwrap_or(i64::MAX));
        peers::mark_unregistered(tx, &sid, now)?;
    } else if prev.unregistered_at_timestamp > 0 {
        patch.unregistered_at = Patch::Clear;
        peers::mark_registered(tx, &sid, now)?;
    }
    if let Some(number) = e164 {
        patch.identifiers = Some(vec![Identifier {
            kind: IdentifierKind::Phone,
            value: number.to_owned(),
        }]);
    }
    let identity_events = if aci.is_some() {
        merge_identity(tx, merge, &sid, record, &mut patch)?
    } else {
        Vec::new()
    };

    peers::set_profile_sharing(tx, &sid, record.whitelisted, now)?;
    // The address-book NAME (given or family) makes a system contact; a system nickname alone
    // only names the person (Desktop's `isInSystemContacts`; P1 F5).
    let in_address_book =
        join_name(&record.system_given_name, &record.system_family_name).is_some();
    peers::set_system_contact(tx, &sid, in_address_book, now)?;
    peers::set_blocked(tx, &sid, record.blocked, now)?;
    let after = peers::load(tx, &sid)?;
    // The record may have changed any input of the number rule (the key, the address-book name).
    patch.number_shown = Some(peers::number_shown(tx, &sid)?);
    merge.events.push(BackendEvent::ContactUpsert(patch));
    merge.events.extend(identity_events);

    // The 1:1 chat: only for roaming changes, or a changed accepted state of a chat the core has.
    // The FIRST record of a contact whose chat the core has (a history import made it, D10)
    // states the flags whatever they are: the sync wins over the archive, also where the
    // archive said true and the record says false. For a first record of a contact the core
    // has NO chat for, the flags are stated only when one differs from the default, as
    // before: a `ConversationUpsert` makes the conversation, so stating them for every
    // contact of the Storage Service (most are people with no chat) would make a chat of each.
    // The archived flag is diffed against what the core was LAST TOLD (`peers.roaming_archived`,
    // moved by the unarchive a new message makes), else against the previous record: with only
    // the record to compare, a chat the phone archives again after Kourier brought it back
    // (the record says archived, as the one before it did) would never be archived here.
    //
    // The three-way rule (A10, §3.3): a field the user changed here that the phone has not
    // written yet (an intent) is not diffed and not stated while the phone's record leaves it
    // alone, and the told base keeps OUR value; where the phone wrote something else it wins
    // (the field is stated whether or not it differs from the base, and the base follows).
    let muted = muted_until(record.muted_until_timestamp);
    let told_archived = before.roaming.map_or(prev.archived, |r| r.archived);
    let settled = merge.settle(
        tx,
        &contact_target(&sid),
        intents::Flags {
            archived: record.archived,
            marked_unread: record.marked_unread,
            muted_until: muted,
        },
        intents::Flags {
            archived: told_archived,
            marked_unread: prev.marked_unread,
            muted_until: before
                .roaming
                .map_or(muted_until(prev.muted_until_timestamp), |r| r.muted_until),
        },
    )?;
    let roaming_changed = settled.archived.states(record.archived != told_archived)
        || settled
            .marked_unread
            .states(record.marked_unread != prev.marked_unread)
        || settled
            .muted_until
            .states(muted != muted_until(prev.muted_until_timestamp))
        || record.blocked != prev.blocked
        || (first_record && after.features_hash.is_some());
    let accepted_changed = before.accepted() != after.accepted() && after.features_hash.is_some();
    if roaming_changed || accepted_changed {
        let mut conv = ConversationPatch::new(ConvKey::Direct(user));
        if roaming_changed {
            if settled.archived.states(true) {
                conv.archived = Some(record.archived);
            }
            if settled.marked_unread.states(true) {
                conv.marked_unread = Some(record.marked_unread);
            }
            if settled.muted_until.states(true) {
                conv.muted_until = muted.map_or(Patch::Clear, Patch::Set);
            }
            conv.blocked = Some(record.blocked);
            peers::set_roaming(
                tx,
                &sid,
                Roaming {
                    archived: settled.archived.flag(record.archived),
                    muted_until: settled.muted_until.mute(muted),
                },
                now,
            )?;
        }
        merge.events.push(BackendEvent::ConversationUpsert(conv));
    }
    Ok(Some(sid))
}

/// Whether a profile fetch of `sid` succeeded before.
fn profile_fetched(conn: &Connection, sid: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT profile_fetched_at IS NOT NULL FROM peers WHERE service_id = ?1")?
        .query_row([sid], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}

/// `identities.verified` values (see `store::identity`).
const VERIFIED_DEFAULT: i64 = 0;
const VERIFIED: i64 = 1;
const VERIFIED_UNVERIFIED: i64 = 2;

/// A contact record's identity key and verified state (Desktop: applied when we have no key or
/// the same key; a different key waits for a profile fetch, which is authoritative), through
/// [`crate::safety::apply_remote_state`]: a row that leaves state 2 is released and a changed
/// state gets the quiet "... from another device" notice (P5 critic H1, M2.2-M2.4); a record
/// while this device's own Verified sync for the same key is still queued is older than the
/// user's click here and changes nothing. Returns the events to append after the contact's
/// upsert.
fn merge_identity(
    tx: &Connection,
    merge: &mut Merge<'_>,
    sid: &str,
    record: &proto::ContactRecord,
    patch: &mut ContactPatch,
) -> Result<Vec<BackendEvent>> {
    let Ok(remote) = IdentityKey::decode(&record.identity_key) else {
        return Ok(Vec::new());
    };
    // A10: a click here that the phone has not written yet (an identity intent of this very key)
    // is newer than a record that still holds the state it had (`theirs == base`): the record is
    // not applied. The phone's other state is applied like any record (an intent that it made
    // moot is gone); another key's intent was dropped by the rule's lookup.
    if merge.intents
        && intents::settle_identity(
            tx,
            &contact_target(sid),
            &record.identity_key,
            record.identity_state,
        )? == Some(intents::Verdict::Stays)
    {
        return Ok(Vec::new());
    }
    let verified = match proto::contact_record::IdentityState::try_from(record.identity_state) {
        Ok(proto::contact_record::IdentityState::Verified) => VERIFIED,
        Ok(proto::contact_record::IdentityState::Unverified) => VERIFIED_UNVERIFIED,
        _ => VERIFIED_DEFAULT,
    };
    match crate::safety::apply_remote_state(
        tx,
        &patch.user.clone(),
        sid,
        &remote,
        verified,
        merge.now,
        crate::safety::RemoteSource::Storage,
    )? {
        crate::safety::Remote::OtherKey => {
            jobs::request(tx, JobKind::ProfileFetch, sid, merge.now, None)?;
            merge.outcome.profile_fetches = true;
            Ok(Vec::new())
        }
        crate::safety::Remote::Kept => Ok(Vec::new()),
        crate::safety::Remote::Applied {
            trust,
            fingerprint,
            events,
            released,
        } => {
            patch.identity_fingerprint = Patch::Set(fingerprint);
            patch.trust = Some(trust);
            merge.outcome.released |= released;
            Ok(events)
        }
    }
}

/// `GroupV2Record` (`mergeGroupV2Record`): the master key and the roaming flags. A group never
/// fetched gets a `group_refresh` job (its state joins the core with its first upsert, flags
/// included); a group the core has gets the changed flags at once.
fn merge_group(
    tx: &Connection,
    merge: &mut Merge<'_>,
    record: &proto::GroupV2Record,
) -> Result<()> {
    let Some(keys) = crate::groups::GroupKeys::from_slice(&record.master_key) else {
        return Ok(());
    };
    let now = merge.now;
    let before = crate::groups::load_by_master(tx, &keys.master)?;
    // The three-way rule (A10, §3.3, critic M3): the `groups` columns are the mirror of what the
    // core was told, and the merge used to overwrite them from the record. A field with an
    // intent that stays keeps OUR value in the column (and is not stated); where the phone wins
    // the column takes the record's value and the field is stated.
    let muted = muted_until(record.muted_until_timestamp);
    let theirs = intents::Flags {
        archived: record.archived,
        marked_unread: record.marked_unread,
        muted_until: muted,
    };
    let told = before.as_ref().map_or(theirs, |b| intents::Flags {
        archived: b.archived,
        marked_unread: b.marked_unread,
        muted_until: b.muted_until,
    });
    let settled = merge.settle(tx, &group_target(&keys.master), theirs, told)?;
    let archived = settled.archived.flag(record.archived);
    let marked_unread = settled.marked_unread.flag(record.marked_unread);
    let mirror_muted = settled.muted_until.mute(muted);
    tx.prepare_cached(
        "INSERT INTO groups(master_key, group_id, blocked, whitelisted, archived, marked_unread,
           muted_until, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(master_key) DO UPDATE SET blocked = excluded.blocked,
           whitelisted = excluded.whitelisted, archived = excluded.archived,
           marked_unread = excluded.marked_unread, muted_until = excluded.muted_until,
           updated_at = excluded.updated_at",
    )?
    .execute((
        &keys.master[..],
        &keys.id[..],
        record.blocked,
        record.whitelisted,
        archived,
        marked_unread,
        mirror_muted,
        now,
    ))?;
    match before {
        Some(before) if before.features_hash.is_some() => {
            if before.blocked != record.blocked
                || settled.archived.states(before.archived != archived)
                || settled
                    .marked_unread
                    .states(before.marked_unread != marked_unread)
                || settled
                    .muted_until
                    .states(before.muted_until != mirror_muted)
            {
                let mut patch = ConversationPatch::new(keys.conv_key());
                patch.blocked = Some(record.blocked);
                if settled.archived.states(true) {
                    patch.archived = Some(record.archived);
                }
                if settled.marked_unread.states(true) {
                    patch.marked_unread = Some(record.marked_unread);
                }
                if settled.muted_until.states(true) {
                    patch.muted_until = muted.map_or(Patch::Clear, Patch::Set);
                }
                merge.events.push(BackendEvent::ConversationUpsert(patch));
            }
        }
        Some(before) if before.snapshot.is_some() => {}
        _ => {
            jobs::enqueue(
                tx,
                JobKind::GroupRefresh,
                &crate::groups::job_key(&keys.id),
                now,
                None,
            )?;
            merge.outcome.group_refreshes = true;
        }
    }
    Ok(())
}

/// `AccountRecord` → our profile key, synced settings, Note to Self state and pins.
fn merge_account(
    tx: &Connection,
    merge: &mut Merge<'_>,
    key: &[u8],
    record: &proto::AccountRecord,
) -> Result<()> {
    use proto::account_record::PhoneNumberSharingMode;

    let account = merge.account;
    let prev = previous_account(tx, key)?;
    let first_record = prev.is_none();
    let prev = prev.unwrap_or_default();
    if record.profile_key.len() == 32 && record.profile_key[..] != account.profile_key[..] {
        tx.execute(
            "UPDATE account SET profile_key = ?1 WHERE id = 1",
            [&record.profile_key[..]],
        )?;
        merge.outcome.own_profile_key = true;
    }
    let own = aci_user_id(account.aci).map_err(|_| Error::Internal("own ACI user id"))?;
    if let Some(name) = join_name(&record.given_name, &record.family_name) {
        let mut patch = ContactPatch::new(own.clone());
        patch.profile_name = Patch::Set(name);
        merge.events.push(BackendEvent::ContactUpsert(patch));
    }

    // Synced settings. An unset sharing mode decodes as UNKNOWN, which Desktop treats as
    // "nobody" (only an absent field means "everybody" there; prost cannot tell them apart, and
    // "nobody" is the private choice).
    let everybody = record.phone_number_sharing_mode == PhoneNumberSharingMode::Everybody as i32;
    db::store_configuration(
        tx,
        Some(record.read_receipts),
        Some(record.typing_indicators),
    )?;
    tx.execute(
        "UPDATE account SET phone_number_sharing = ?1, keep_muted_archived = ?2 WHERE id = 1",
        (
            if everybody { "everybody" } else { "nobody" },
            record.keep_muted_chats_archived,
        ),
    )?;
    merge
        .events
        .push(BackendEvent::AccountSettings(AccountSettingsPatch {
            read_receipts: Some(record.read_receipts),
            typing_indicators: Some(record.typing_indicators),
            link_previews: Some(record.link_previews),
            sealed_sender_indicators: Some(record.sealed_sender_indicators),
            universal_timer: if record.universal_expire_timer > 0 {
                Patch::Set(Duration::from_secs(u64::from(
                    record.universal_expire_timer,
                )))
            } else {
                Patch::Clear
            },
            share_phone_number: Patch::Set(everybody),
        }));

    // Which picture a contact shows (`preferContactAvatars`). Each contact has one avatar
    // here, of the source the setting prefers when both exist; after a change the other
    // source is asked again: the phone for its address-book photos, or the profiles of the
    // contacts that show such a photo.
    if record.prefer_contact_avatars != prev.prefer_contact_avatars {
        if record.prefer_contact_avatars {
            jobs::enqueue(
                tx,
                JobKind::SyncRequest,
                jobs::SyncRequest::Contacts.as_str(),
                merge.now,
                Some(jobs::SYNC_REQUEST_MAX_AGE),
            )?;
        } else {
            let shown: Vec<String> = tx
                .prepare_cached(
                    "SELECT service_id FROM peers WHERE avatar_source = 'contact_sync'",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            for sid in shown {
                crate::profile::request(tx, &sid, merge.now)?;
            }
        }
    }

    // Note to Self. The first account record states its flags when the core has the chat (a
    // history import made it): the sync wins over the archive (D10), as for a contact. When
    // the core has no such chat the flags are not sent for nothing: an upsert makes the chat
    // (the same reason as for a contact, where every Storage contact would become a chat).
    // The archived flag is diffed against what the core was last told (`peers.roaming_archived`
    // of our own row, as for a contact), else against the previous record.
    // The three-way rule (A10, §3.3) on the archive and mark intents of the account record's
    // target, exactly as for a contact (Note to Self has no mute).
    let own_sid = ServiceId::from(account.aci).service_id_string();
    let own_peer = peers::load(tx, &own_sid)?;
    let told_archived = own_peer
        .roaming
        .map_or(prev.note_to_self_archived, |r| r.archived);
    let settled = merge.settle(
        tx,
        ACCOUNT_TARGET,
        intents::Flags {
            archived: record.note_to_self_archived,
            marked_unread: record.note_to_self_marked_unread,
            muted_until: None,
        },
        intents::Flags {
            archived: told_archived,
            marked_unread: prev.note_to_self_marked_unread,
            muted_until: None,
        },
    )?;
    if settled
        .archived
        .states(record.note_to_self_archived != told_archived)
        || settled
            .marked_unread
            .states(record.note_to_self_marked_unread != prev.note_to_self_marked_unread)
        || (first_record && own_peer.features_hash.is_some())
    {
        let mut conv = ConversationPatch::new(ConvKey::Direct(own.clone()));
        if settled.archived.states(true) {
            conv.archived = Some(record.note_to_self_archived);
        }
        if settled.marked_unread.states(true) {
            conv.marked_unread = Some(record.note_to_self_marked_unread);
        }
        merge.events.push(BackendEvent::ConversationUpsert(conv));
        peers::set_roaming(
            tx,
            &own_sid,
            Roaming {
                archived: settled.archived.flag(record.note_to_self_archived),
                muted_until: None,
            },
            merge.now,
        )?;
    }

    // Pins, the record's order across kinds (`pinned_chats`). A 1:1 chat is made first with a
    // bare upsert, so that it exists with its features (`with_features`). The three-way rule
    // (A10, §3.3) runs on the pin intents first: one that stays is applied to the list the core
    // is told (our removals dropped, our additions last), one the phone settled is gone.
    let held_pins = if merge.intents {
        intents::settle_pins(tx, record)?
    } else {
        None
    };
    let pinned = pinned_chats(tx, held_pins.as_ref().unwrap_or(record))?;
    for pin in pinned.iter().filter(|pin| pin.contact) {
        merge
            .events
            .push(BackendEvent::ConversationUpsert(ConversationPatch::new(
                pin.conv.clone(),
            )));
    }
    merge.events.push(BackendEvent::PinnedConversations(
        pinned.into_iter().map(|pin| pin.conv).collect(),
    ));
    Ok(())
}

/// A chat an account record pins.
struct Pinned {
    conv: ConvKey,
    /// A 1:1 chat (a group is named only when the core has it already).
    contact: bool,
}

/// The chats an account record pins, in the record's order across kinds, each once: 1:1
/// chats by service id, groups by master key. The core makes a chat of every key the list
/// names, so a group is named only when the core has it (its features were sent:
/// `groups.features_hash`); a group it does not have would be a bare chat without a title or
/// members. A person known only by a number is left out (no account to name; such a chat is
/// read-only). The one place that reads a pin list: the sync that merges the record
/// ([`merge_account`]) and the first fetch of a group the record pins
/// ([`pins_with_new_group`]).
fn pinned_chats(conn: &Connection, record: &proto::AccountRecord) -> Result<Vec<Pinned>> {
    use proto::account_record::pinned_conversation::Identifier as Pin;

    let mut pinned: Vec<Pinned> = Vec::new();
    for pin in &record.pinned_conversations {
        let (conv, contact) = match &pin.identifier {
            Some(Pin::Contact(contact)) => {
                let service_id = service_id_from_wire(
                    Some(contact.service_id_binary.as_slice()),
                    Some(contact.service_id.as_str()),
                )
                .ok()
                .flatten();
                let Some(user) = service_id.and_then(|s| service_id_user_id(s).ok()) else {
                    continue;
                };
                (ConvKey::Direct(user), true)
            }
            Some(Pin::GroupMasterKey(master)) => {
                let Some(keys) = crate::groups::GroupKeys::from_slice(master) else {
                    continue;
                };
                let in_core = crate::groups::load_by_master(conn, &keys.master)?
                    .is_some_and(|row| row.features_hash.is_some());
                if !in_core {
                    continue;
                }
                (keys.conv_key(), false)
            }
            _ => continue,
        };
        if !pinned.iter().any(|pin| pin.conv == conv) {
            pinned.push(Pinned { conv, contact });
        }
    }
    Ok(pinned)
}

/// The pin list for a group whose first fetch has just given the core its features
/// (`groups::apply`, in that zone): when the stored account record pins the group, the whole
/// list again, `None` otherwise. On a fresh link the first sync merges the account record
/// before any group is fetched, so it had to leave the group out; the phone does not write
/// its account record again for days, and nothing else would pin the group. The list is the
/// record's, in its order, and names only chats the core has (a 1:1 chat that the first
/// sync made, a group fetched before): it is the list the sync would send now, so it agrees
/// with the sync's and with the history import's (the phone's order). The core's pin list
/// is a full, idempotent list.
pub(crate) fn pins_with_new_group(
    conn: &Connection,
    group: &crate::groups::GroupKeys,
) -> Result<Option<BackendEvent>> {
    let raw: Option<Vec<u8>> = conn
        .prepare_cached(
            "SELECT record FROM storage_records WHERE kind = ?1 ORDER BY updated_at DESC, rowid DESC
             LIMIT 1",
        )?
        .query_row([ItemType::Account as i32], |r| r.get(0))
        .optional()?;
    let Some(Record::Account(record)) = raw
        .and_then(|raw| proto::StorageRecord::decode(&raw[..]).ok())
        .and_then(|record| record.record)
    else {
        return Ok(None);
    };
    // A pin or an unpin made here that has not reached the phone is still in force (A10).
    let held = intents::with_live_pins(conn, &record)?;
    let pinned = pinned_chats(conn, held.as_ref().unwrap_or(&record))?;
    let group = group.conv_key();
    if !pinned.iter().any(|pin| pin.conv == group) {
        return Ok(None);
    }
    let mut list = Vec::with_capacity(pinned.len());
    for pin in pinned {
        // A 1:1 chat the core does not have is not named (nothing makes it here).
        if pin.contact
            && let ConvKey::Direct(user) = &pin.conv
            && features::core_has_direct(conn, user)? != Some(true)
        {
            continue;
        }
        list.push(pin.conv);
    }
    Ok(Some(BackendEvent::PinnedConversations(list)))
}

/// `SyncMessage.Keys` from our phone (`SD/ts/textsecure/MessageReceiver.preload.ts`
/// `#handleKeys`): a new account entropy pool or master key; a changed storage key restarts the
/// Storage Service from scratch. Returns whether a sync was queued.
pub(crate) fn apply_keys(
    conn: &Connection,
    account_entropy_pool: Option<&str>,
    master: Option<&[u8]>,
    now: i64,
) -> Result<bool> {
    use std::str::FromStr as _;

    use libsignal_account_keys::{AccountEntropyPool, SvrKey};

    let master_key = match account_entropy_pool.filter(|p| !p.is_empty()) {
        Some(pool) => match AccountEntropyPool::from_str(pool) {
            Ok(parsed) => Some(Zeroizing::new(parsed.derive_svr_key())),
            Err(_) => return Ok(false),
        },
        None => master
            .and_then(|m| <[u8; 32]>::try_from(m).ok())
            .map(Zeroizing::new),
    };
    let Some(master_key) = master_key else {
        return Ok(false);
    };
    let storage_key = Zeroizing::new(SvrKey::new(*master_key).derive_storage_service_key());
    let stored: Zeroizing<Vec<u8>> = Zeroizing::new(conn.query_row(
        "SELECT storage_key FROM account WHERE id = 1",
        [],
        |r| r.get(0),
    )?);
    conn.execute(
        "UPDATE account SET master_key = ?1, storage_key = ?2,
           account_entropy_pool = coalesce(?3, account_entropy_pool) WHERE id = 1",
        (
            master_key.as_slice(),
            storage_key.as_slice(),
            account_entropy_pool.filter(|p| !p.is_empty()),
        ),
    )?;
    if stored.as_slice() == storage_key.as_slice() {
        return Ok(false);
    }
    reset(conn, now)?;
    Ok(true)
}

/// Forgets what the Storage Service gave us (a new storage key, a rebuilt `core.db`) and queues
/// a full sync. A write in flight is forgotten with it (critic M4): `storage_generation` moves, so
/// the landing zone of a write planned before (`intents::land`) drops its result, and the
/// in-flight row is deleted. The intents stay: the full sync merges the phone's records against
/// them by the three-way rule again.
pub(crate) fn reset(conn: &Connection, now: i64) -> Result<()> {
    conn.execute("DELETE FROM storage_records", [])?;
    conn.execute(
        "UPDATE account SET manifest_version = NULL, storage_generation = storage_generation + 1
         WHERE id = 1",
        [],
    )?;
    write::delete_in_flight(conn)?;
    // A sync running with the old key runs again after it.
    jobs::request(conn, JobKind::StorageSync, "", now, None)?;
    Ok(())
}

pub(crate) mod intents;
pub(crate) mod wire;
pub(crate) mod write;

#[cfg(test)]
mod tests;
