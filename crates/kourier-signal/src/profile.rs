//! Profiles (§6.10): the versioned fetch with the access key over the unauthenticated socket
//! (falling back to the authenticated one), name, about and emoji decrypted with the profile key,
//! the avatar from CDN0 into the blob store, the identity key and the sealed-sender state.
//!
//! Desktop sources: `SD/ts/services/profiles.preload.ts` (`buildProfileFetchOptions`,
//! `doGetProfile`, `updateProfileFromResponse`: 401/403 with an access key → sealed sender
//! disabled and an authenticated retry, 404 → unregistered), `SD/ts/Crypto.node.ts`
//! (`decryptProfile`, `decryptProfileName`, `verifyAccessKey`),
//! `SD/ts/models/conversations.preload.ts` (`setAndMaybeFetchProfileAvatar`),
//! `SD/ts/routineProfileRefresh.preload.ts` (≤ 50 contacts not refreshed for 3 days, hourly).
//!
//! The fetch and the avatar download happen first; one zone then stores the identity key,
//! sealed-sender state, avatar and fetch time and writes the `ContactUpsert` (§5.6).

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit as _, Mac as _};
use http::{HeaderName, HeaderValue, StatusCode};
use kourier_backend_api::BackendEvent;
use kourier_backend_api::clock::duration_ms;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{BackendBlob, ContactPatch, Patch};
use libsignal_core::{Aci, DeviceId, ServiceId};
use libsignal_protocol::{IdentityKey, IdentityKeyStore as _, ProtocolAddress};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::actor::Shared;
use crate::avatar::{self, Source};
use crate::crypto::decrypt_gcm;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::jobs::{self, Done, Job, JobKind};
use crate::net::{ChatRequest, ChatResponse, HttpsHost, HttpsRequest, SignalTransport, parse_json};
use crate::outbox::append_events;
use crate::peers::{self, ACCESS_KEY_LEN, SealedSenderState, ZERO_ACCESS_KEY};
use crate::receive::stage1::identity_events;
use crate::service_id::aci_user_id;
use crate::store::{IdentityKind, Stores, ready};

/// Route of profile fetches.
const PROFILE_PATH: &str = "/v1/profile";
/// The access-key header of unauthenticated requests.
const ACCESS_KEY_HEADER: &str = "unidentified-access-key";
/// Profiles older than this are refreshed (`routineProfileRefresh`).
const REFRESH_AFTER: Duration = Duration::from_secs(3 * 24 * 3600);
/// Most profiles one refresh queues.
const REFRESH_BATCH: i64 = 50;
/// How often the refresh runs.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
/// Timeout of the avatar download.
const AVATAR_TIMEOUT: Duration = Duration::from_secs(30);
/// `BackendBlob.v` of the profile fields Kourier's contract has no field for (`Contact.meta`).
pub(crate) const PROFILE_META_V: u16 = 1;

/// The answer of `GET /v1/profile/…` (the fields Kourier uses).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ProfileBody {
    identity_key: Option<String>,
    name: Option<String>,
    about: Option<String>,
    about_emoji: Option<String>,
    avatar: Option<String>,
    unidentified_access: Option<String>,
    unrestricted_unidentified_access: bool,
}

/// `about` and `aboutEmoji`, kept in `Contact.meta` (the contract has no field for them yet).
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProfileMeta {
    /// The "about" text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) about: Option<String>,
    /// The emoji next to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) about_emoji: Option<String>,
}

/// Decrypts a padded profile field and strips the zero padding.
fn decrypt_field(key: &[u8; 32], b64: &str) -> Option<Zeroizing<Vec<u8>>> {
    let data = Zeroizing::new(crate::wire_b64::LENIENT.decode(b64).ok()?);
    decrypt_gcm(key, &data).ok()
}

/// The profile name: `given \0 family`, zero padded (`decryptProfileName`), as "Given Family".
pub(crate) fn decrypt_name(key: &[u8; 32], b64: &str) -> Option<String> {
    let padded = decrypt_field(key, b64)?;
    let mut parts = padded.split(|b| *b == 0);
    let given = String::from_utf8_lossy(parts.next().unwrap_or(&[]))
        .trim()
        .to_owned();
    let family = String::from_utf8_lossy(parts.next().unwrap_or(&[]))
        .trim()
        .to_owned();
    match (given.is_empty(), family.is_empty()) {
        (true, true) => None,
        (false, true) => Some(given),
        (true, false) => Some(family),
        (false, false) => Some(format!("{given} {family}")),
    }
}

/// A padded text field (`about`, `aboutEmoji`).
pub(crate) fn decrypt_text(key: &[u8; 32], b64: &str) -> Option<String> {
    let padded = decrypt_field(key, b64)?;
    let end = padded.iter().position(|b| *b == 0).unwrap_or(padded.len());
    let text = String::from_utf8_lossy(&padded[..end]).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// `verifyAccessKey`: the profile's `unidentifiedAccess` is HMAC-SHA256(accessKey, 32 zeros).
pub(crate) fn verify_access_key(access_key: &[u8; ACCESS_KEY_LEN], verifier_b64: &str) -> bool {
    let Ok(verifier) = crate::wire_b64::LENIENT.decode(verifier_b64) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(access_key) else {
        return false;
    };
    mac.update(&[0u8; 32]);
    mac.verify_slice(&verifier).is_ok()
}

/// The sealed-sender state a profile answer implies (`updateProfileFromResponse`).
fn sealed_state(
    body: &ProfileBody,
    access_key: Option<&[u8; ACCESS_KEY_LEN]>,
) -> SealedSenderState {
    match &body.unidentified_access {
        Some(_) if body.unrestricted_unidentified_access => SealedSenderState::Unrestricted,
        Some(verifier) if access_key.is_some_and(|key| verify_access_key(key, verifier)) => {
            SealedSenderState::Enabled
        }
        _ => SealedSenderState::Disabled,
    }
}

/// Queues a profile fetch of `service_id` (a no-op when one is queued).
pub(crate) fn queue(conn: &Connection, service_id: &str, now: i64) -> Result<bool> {
    jobs::enqueue(conn, JobKind::ProfileFetch, service_id, now, None)
}

/// Asks for a profile fetch of `service_id` because something changed (a new profile key, the
/// user's refresh): due now, and once more after a fetch that is running (`jobs::request`).
pub(crate) fn request(conn: &Connection, service_id: &str, now: i64) -> Result<()> {
    jobs::request(conn, JobKind::ProfileFetch, service_id, now, None)
}

/// Whether `service_id`'s profile was fetched before.
pub(crate) fn fetched(conn: &Connection, service_id: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT profile_fetched_at IS NOT NULL FROM peers WHERE service_id = ?1")?
        .query_row([service_id], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}

/// What we know about the peer before fetching.
struct Known {
    profile_key: Option<Zeroizing<[u8; 32]>>,
    access_key: Option<[u8; ACCESS_KEY_LEN]>,
    sealed: SealedSenderState,
    avatar_path: Option<String>,
    avatar_source: Option<Source>,
    /// The phone's setting: address-book photos win over profile pictures
    /// ([`crate::storage::prefer_contact_avatars`]).
    prefer_contact: bool,
}

fn known(conn: &Connection, sid: &str) -> Result<Known> {
    let peer = peers::load(conn, sid)?;
    let row: Option<(Option<String>, Option<String>)> = conn
        .prepare_cached("SELECT profile_avatar, avatar_source FROM peers WHERE service_id = ?1")?
        .query_row([sid], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (avatar_path, source) = row.unwrap_or((None, None));
    Ok(Known {
        profile_key: peers::profile_key(conn, sid)?,
        access_key: peer.access_key,
        sealed: peer.sealed,
        avatar_path,
        avatar_source: match source.as_deref() {
            Some("profile") => Some(Source::Profile),
            Some("contact_sync") => Some(Source::ContactSync),
            _ => None,
        },
        prefer_contact: crate::storage::prefer_contact_avatars(conn)?,
    })
}

/// The `profile_fetch` job for `job.key` (an ACI service id string).
pub(crate) async fn run_fetch<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(&job.key) else {
        return Ok(Done::Finished);
    };
    if aci == account.aci {
        return Ok(Done::Finished);
    }
    let sid = job.key.clone();
    let known = shared.db.read(|c| known(c, &sid))?;
    let response = fetch(shared, socket, aci, &known).await?;
    match response.status {
        StatusCode::NOT_FOUND => {
            let now = shared.now();
            shared
                .db
                .zone(|tx| peers::mark_unregistered(tx, &sid, now))?;
            return Ok(Done::Finished);
        }
        status if !status.is_success() => {
            return Err(Error::Status {
                what: "profile",
                status: status.as_u16(),
            });
        }
        _ => {}
    }
    let body: ProfileBody = parse_json(&response)?;
    let avatar = fetch_avatar(shared, &known, &body).await;
    let now = shared.now();
    let result = shared.db.zone(|tx| {
        apply(
            tx,
            &account,
            &shared.blob_dir,
            aci,
            &known,
            &body,
            avatar.as_ref(),
            now,
            &shared.wake,
        )
    });
    if result.is_err()
        && let Some(AvatarChange::Set { written, .. }) = &avatar
    {
        avatar::discard(&shared.blob_dir, written);
    }
    result?;
    kourier_db::failpoint::hit(crate::receive::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
    Ok(Done::Finished)
}

/// The versioned fetch with the access key (unauthenticated), else authenticated
/// (`buildProfileFetchOptions` steps 1 and 5).
async fn fetch<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
    aci: Aci,
    known: &Known,
) -> Result<ChatResponse> {
    let service_id = ServiceId::from(aci).service_id_string();
    let path = match &known.profile_key {
        Some(key) => {
            let version = zkgroup::profiles::ProfileKey::create(**key).get_profile_key_version(aci);
            format!("{PROFILE_PATH}/{service_id}/{}", version.as_ref())
        }
        None => format!("{PROFILE_PATH}/{service_id}"),
    };
    let access = match known.sealed {
        SealedSenderState::Disabled => None,
        SealedSenderState::Unrestricted => Some(ZERO_ACCESS_KEY),
        SealedSenderState::Enabled | SealedSenderState::Unknown => known.access_key,
    };
    if let Some(access) = access.filter(|_| known.profile_key.is_some()) {
        let mut value = HeaderValue::from_str(&STANDARD.encode(access))
            .map_err(|_| Error::Internal("access key is not a header value"))?;
        value.set_sensitive(true);
        let request =
            ChatRequest::get(&path)?.with_header(HeaderName::from_static(ACCESS_KEY_HEADER), value);
        let unauth = shared.unauth.get(&shared.net, shared.now_ms()).await?;
        match shared.net.request(&unauth, request).await {
            Ok(response)
                if matches!(
                    response.status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
            {
                // The access key no longer works: sealed sender off, then authenticated.
                let now = shared.now();
                shared.db.zone(|tx| {
                    peers::set_sealed_state(tx, &service_id, SealedSenderState::Disabled, now)
                })?;
            }
            Ok(response) => return Ok(response),
            Err(e) => {
                shared.unauth.invalidate(&unauth);
                return Err(e.into());
            }
        }
    }
    Ok(shared.net.request(socket, ChatRequest::get(&path)?).await?)
}

/// What the profile says about the avatar.
enum AvatarChange {
    /// A new avatar file (not recorded yet).
    Set {
        written: avatar::Written,
        path: String,
    },
    /// The profile has no avatar any more.
    Clear,
}

/// CDN0 avatar paths are server-generated (`profiles/<id>`): no `..`, no leading slash.
fn valid_avatar_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 256
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'='))
}

/// Downloads and decrypts a new profile avatar into a blob file (network and file I/O before
/// the zone). `None`: nothing to change (or the download failed; the next refresh retries).
async fn fetch_avatar<T: SignalTransport>(
    shared: &Shared<T>,
    known: &Known,
    body: &ProfileBody,
) -> Option<AvatarChange> {
    let key = known.profile_key.as_ref()?;
    // The contact shows the photo from our phone's address book and the setting prefers it:
    // the profile picture does not replace it (Desktop's `getAvatar`).
    if known.prefer_contact && known.avatar_source == Some(Source::ContactSync) {
        return None;
    }
    let Some(path) = body.avatar.as_deref().filter(|p| !p.is_empty()) else {
        return (known.avatar_source == Some(Source::Profile)).then_some(AvatarChange::Clear);
    };
    if known.avatar_source == Some(Source::Profile) && known.avatar_path.as_deref() == Some(path) {
        return None;
    }
    if !valid_avatar_path(path) {
        tracing::warn!("profile avatar path refused");
        return None;
    }
    let mut request = HttpsRequest::get(HttpsHost::Cdn(0), format!("/{path}"));
    request.timeout = AVATAR_TIMEOUT;
    request.max_body = avatar::MAX_AVATAR_LEN;
    let response = match shared.net.https(request).await {
        Ok(response) if response.status.is_success() => response,
        Ok(response) => {
            tracing::warn!(
                status = response.status.as_u16(),
                "profile avatar download failed"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "profile avatar download failed");
            return None;
        }
    };
    let Ok(image) = decrypt_gcm(key, &response.body) else {
        tracing::warn!("profile avatar did not decrypt (profile key changed?)");
        return None;
    };
    match avatar::write(&shared.blob_dir, &image) {
        Ok(written) => Some(AvatarChange::Set {
            written,
            path: path.to_owned(),
        }),
        Err(e) => {
            tracing::warn!(error = %e, "storing a profile avatar failed");
            None
        }
    }
}

/// The zone of one fetched profile.
#[allow(clippy::too_many_arguments, reason = "one zone's inputs, called once")]
fn apply(
    tx: &mut kourier_db::Tx<'_>,
    account: &Account,
    blob_dir: &std::path::Path,
    aci: Aci,
    known: &Known,
    body: &ProfileBody,
    avatar: Option<&AvatarChange>,
    now: i64,
    wake: &Rc<tokio::sync::Notify>,
) -> Result<()> {
    let sid = ServiceId::from(aci).service_id_string();
    let user = aci_user_id(aci).map_err(|_| Error::Internal("ACI user id"))?;
    let mut events = Vec::new();
    // The identity key (`updateIdentityKey`): a changed key archives sibling sessions and is
    // reported like one learned from a message.
    if let Some(key) = body
        .identity_key
        .as_deref()
        .and_then(|k| crate::wire_b64::LENIENT.decode(k).ok())
        .and_then(|k| IdentityKey::decode(&k).ok())
    {
        let mut stores = Stores::new(
            tx,
            IdentityKind::Aci,
            account.aci_identity,
            account.aci_registration_id,
            now,
        );
        let device = DeviceId::new(1).map_err(|_| Error::Internal("device id"))?;
        let address = ProtocolAddress::new(sid.clone(), device);
        ready(stores.identity.save_identity(&address, &key))??;
        let changes = stores.identity.take_changes();
        events.extend(identity_events(tx, &changes, now)?);
    }
    // Sealed sender, from the freshest access key (`c.get('accessKey')`).
    let access_key = peers::load(tx, &sid)?.access_key;
    peers::set_sealed_state(tx, &sid, sealed_state(body, access_key.as_ref()), now)?;

    let mut patch = ContactPatch::new(user);
    if let Some(key) = &known.profile_key {
        if let Some(name) = body.name.as_deref().and_then(|n| decrypt_name(key, n)) {
            patch.profile_name = Patch::Set(name);
        }
        let meta = ProfileMeta {
            about: body.about.as_deref().and_then(|a| decrypt_text(key, a)),
            about_emoji: body
                .about_emoji
                .as_deref()
                .and_then(|a| decrypt_text(key, a)),
        };
        patch.meta = if meta == ProfileMeta::default() {
            Patch::Clear
        } else {
            Patch::Set(BackendBlob::new(
                PROFILE_META_V,
                serde_json::to_vec(&meta).map_err(|_| Error::Internal("profile meta"))?,
            ))
        };
    }
    match avatar {
        Some(AvatarChange::Set { written, path }) => {
            patch.avatar = avatar::record(
                tx,
                blob_dir,
                &sid,
                &avatar::Change {
                    written: Some(written),
                    source: Source::Profile,
                    profile_path: Some(path),
                    hash: None,
                },
                now,
            )?;
        }
        Some(AvatarChange::Clear) => {
            patch.avatar = avatar::record(
                tx,
                blob_dir,
                &sid,
                &avatar::Change {
                    written: None,
                    source: Source::Profile,
                    profile_path: None,
                    hash: None,
                },
                now,
            )?;
        }
        None => {}
    }
    peers::mark_registered(tx, &sid, now)?;
    tx.prepare_cached("UPDATE peers SET profile_fetched_at = ?2 WHERE service_id = ?1")?
        .execute((&sid, now))?;
    events.push(BackendEvent::ContactUpsert(patch));
    append_events(tx, &events, now, wake)?;
    Ok(())
}

/// The periodic `profile_refresh` job: queues fetches of ≤ 50 contacts with a profile key whose
/// profile is older than 3 days (or was never fetched).
pub(crate) fn refresh<T: SignalTransport>(shared: &Shared<T>, own: Aci) -> Result<Done> {
    let now = shared.now();
    let stale_before = now.saturating_sub(i64::try_from(duration_ms(REFRESH_AFTER)).unwrap_or(0));
    let own = ServiceId::from(own).service_id_string();
    let queued = shared.db.zone(|tx| -> Result<usize> {
        let due: Vec<String> = tx
            .prepare_cached(
                "SELECT service_id FROM peers
                 WHERE profile_key IS NOT NULL AND service_id != ?1
                   AND (profile_fetched_at IS NULL OR profile_fetched_at < ?2)
                 ORDER BY coalesce(profile_fetched_at, 0) LIMIT ?3",
            )?
            .query_map((&own, stale_before, REFRESH_BATCH), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for sid in &due {
            queue(tx, sid, now)?;
        }
        Ok(due.len())
    })?;
    if queued > 0 {
        tracing::debug!(queued, "routine profile refresh");
        shared.jobs_wake.notify_one();
    }
    Ok(Done::RepeatAt(now.saturating_add(
        i64::try_from(duration_ms(REFRESH_INTERVAL)).unwrap_or(i64::MAX),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::encrypt_gcm;

    fn sealed_field(key: &[u8; 32], plain: &[u8], padded_len: usize) -> String {
        let mut padded = plain.to_vec();
        padded.resize(padded_len.max(plain.len()), 0);
        STANDARD.encode(encrypt_gcm(key, [3; 12], &padded))
    }

    /// Desktop's name format (`Crypto_test.preload.ts` "handles a string with family/given
    /// name"): `given \0 family`, padded to 53 bytes; the mock server's unpadded form works too.
    #[test]
    fn names_decrypt_with_and_without_family_and_padding() {
        let key = [5u8; 32];
        assert_eq!(
            decrypt_name(&key, &sealed_field(&key, b"Ada\0Lindqvist", 53)).as_deref(),
            Some("Ada Lindqvist")
        );
        assert_eq!(
            decrypt_name(&key, &sealed_field(&key, b"Bruno", 53)).as_deref(),
            Some("Bruno")
        );
        assert_eq!(
            decrypt_name(&key, &sealed_field(&key, b"Chiara Velasco", 0)).as_deref(),
            Some("Chiara Velasco")
        );
        assert_eq!(decrypt_name(&key, &sealed_field(&key, b"", 53)), None);
        assert_eq!(
            decrypt_name(&[6; 32], &sealed_field(&key, b"Ada", 53)),
            None,
            "wrong key"
        );
        assert_eq!(decrypt_name(&key, "not base64!"), None);
        assert_eq!(
            decrypt_text(&key, &sealed_field(&key, "Out hiking 🥾".as_bytes(), 128)).as_deref(),
            Some("Out hiking 🥾")
        );
    }

    /// The access-key verifier the server stores (`generateAccessKeyVerifier` of the mock,
    /// Desktop's `verifyAccessKey`), with zkgroup's access key of profile key 32 × 0x02.
    #[test]
    fn access_key_verifier_decides_sealed_sender() {
        let access = peers::derive_access_key(&[2; 32]).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&access).unwrap();
        mac.update(&[0; 32]);
        let verifier = STANDARD.encode(mac.finalize().into_bytes());
        assert!(verify_access_key(&access, &verifier));
        assert!(!verify_access_key(&[1; 16], &verifier));
        let body = |access: Option<&str>, unrestricted| ProfileBody {
            unidentified_access: access.map(str::to_owned),
            unrestricted_unidentified_access: unrestricted,
            ..ProfileBody::default()
        };
        assert_eq!(
            sealed_state(&body(Some(&verifier), false), Some(&access)),
            SealedSenderState::Enabled
        );
        assert_eq!(
            sealed_state(&body(Some(&verifier), false), Some(&[1; 16])),
            SealedSenderState::Disabled
        );
        assert_eq!(
            sealed_state(&body(Some(&verifier), false), None),
            SealedSenderState::Disabled
        );
        assert_eq!(
            sealed_state(&body(Some("x"), true), None),
            SealedSenderState::Unrestricted
        );
        assert_eq!(
            sealed_state(&body(None, true), Some(&access)),
            SealedSenderState::Disabled
        );
    }

    #[test]
    fn avatar_paths_stay_on_the_cdn() {
        assert!(valid_avatar_path("profiles/AbC-123_x="));
        assert!(valid_avatar_path("attachments/0f3a"));
        for bad in [
            "",
            "/profiles/x",
            "profiles/../x",
            "a//b",
            "a/./b",
            "a b",
            "x?y",
            "http://e",
        ] {
            assert!(!valid_avatar_path(bad), "{bad}");
        }
    }

    /// zkgroup's profile key version is the path segment of the versioned fetch: 64 hex digits,
    /// different per ACI.
    #[test]
    fn profile_key_versions_are_hex_per_aci() {
        let key = zkgroup::profiles::ProfileKey::create([7; 32]);
        let a = Aci::from(uuid::Uuid::from_u128(1));
        let b = Aci::from(uuid::Uuid::from_u128(2));
        let va = key.get_profile_key_version(a);
        assert_eq!(va.as_ref().len(), 64);
        assert!(va.as_ref().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(va.as_ref(), key.get_profile_key_version(b).as_ref());
    }
}
