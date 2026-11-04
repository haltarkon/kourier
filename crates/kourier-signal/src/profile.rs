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
use kourier_model::{ContactPatch, Patch};
use libsignal_core::{Aci, DeviceId, ServiceId};
use libsignal_protocol::{IdentityKey, IdentityKeyStore as _, ProtocolAddress};
use serde::Deserialize;
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
/// A person's page asks for a fresh profile (`Command::RefreshContact`), but never for one
/// fetched more recently than this (people pages P1, his "no mass fetch" rule: opening pages
/// one after another must not become a profile request each; the routine refresh stays at 3 days).
pub(crate) const PAGE_REFRESH_AFTER: Duration = Duration::from_secs(24 * 3600);

/// A person's page does not ask again for a profile whose fetch was TRIED (and did not succeed)
/// less than this long ago, and our own fetch is not queued again before it either (P1 F6).
pub(crate) const FAILED_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// The routine refresh does not queue a person again within this time of a failed attempt.
const ROUTINE_RETRY_AFTER: Duration = Duration::from_secs(24 * 3600);
/// Most profiles one refresh queues.
const REFRESH_BATCH: i64 = 50;
/// How often the refresh runs.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
/// Timeout of the avatar download.
const AVATAR_TIMEOUT: Duration = Duration::from_secs(30);
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
    /// Whether they share their number, encrypted with their profile key.
    phone_number_sharing: Option<String>,
}

/// The decrypted `phoneNumberSharing`: exactly one byte, 0x01, means the person shares the
/// number; anything else decrypted does not (Desktop `profiles.preload.ts`: "set `sharingPhoneNumber`
/// to `false` in all cases except [0x01]").
pub(crate) fn field_says_sharing(decrypted: &[u8]) -> bool {
    decrypted == [1]
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

/// The user's refresh of one profile (`Command::RefreshContact`, a person's page opened): queues
/// a fetch unless the profile was fetched within [`PAGE_REFRESH_AFTER`], or there is no profile
/// key to decrypt it with (`own_key`: it is our own profile and the account holds the key).
/// Returns whether a fetch was queued.
pub(crate) fn request_throttled(
    conn: &Connection,
    service_id: &str,
    own_key: bool,
    now: i64,
) -> Result<bool> {
    let row: Option<(Option<i64>, Option<i64>, bool)> = conn
        .prepare_cached(
            "SELECT profile_fetched_at, profile_tried_at, profile_key IS NOT NULL
             FROM peers WHERE service_id = ?1",
        )?
        .query_row([service_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let (fetched_at, tried_at, has_key) = row.unwrap_or((None, None, false));
    if !(has_key || own_key) {
        return Ok(false);
    }
    let within = |at: Option<i64>, window: Duration| {
        let window = i64::try_from(duration_ms(window)).unwrap_or(i64::MAX);
        at.is_some_and(|at| at > now.saturating_sub(window) && at <= now)
    };
    // A fetch that succeeded within a day, or one that was tried within the hour (a failing
    // fetch is not repeated at every click).
    if within(fetched_at, PAGE_REFRESH_AFTER) || within(tried_at, FAILED_RETRY_AFTER) {
        return Ok(false);
    }
    request(conn, service_id, now)?;
    Ok(true)
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
    /// The profile is our own (BQ3: his own page shows his about text).
    own: bool,
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
        own: false,
    })
}

/// The `profile_fetch` job for `job.key` (an ACI service id string). Every attempt that ends
/// without a profile (a 404, an error) records its time (`peers.profile_tried_at`; a success
/// clears it), except a rate limit (the hold of the kind answers that) and an error of our own
/// storage (P1 F6).
pub(crate) async fn run_fetch<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let result = fetch_one(shared, socket, job).await;
    if let Err(e) = &result
        && ends_an_attempt(e)
    {
        let now = shared.now();
        let _ = shared.db.zone(|tx| set_tried(tx, &job.key, now));
    }
    result
}

/// Whether `e` ends an attempt of the person's fetch: not a rate limit (the hold of the kind
/// answers that), not an error of our own storage or state.
fn ends_an_attempt(e: &Error) -> bool {
    !matches!(
        e,
        Error::RateLimited { .. } | Error::CoreGone | Error::Db(_) | Error::NotLinked
    )
}

/// The server does not know `service_id` (404): another person is marked unregistered, our own
/// row never is (an account that fetches is registered); the attempt is recorded either way.
fn not_found(conn: &Connection, service_id: &str, own: bool, now: i64) -> Result<()> {
    if !own {
        peers::mark_unregistered(conn, service_id, now)?;
    }
    set_tried(conn, service_id, now)
}

/// Whether our own profile is due: a day after the last attempt that ended, success or not
/// (a failing fetch is not repeated every hour; P1 F6). A success clears `profile_tried_at`,
/// so the later of the two times is the last attempt.
fn own_fetch_due(conn: &Connection, own: &str, now: i64) -> Result<bool> {
    let own_stale = now.saturating_sub(i64::try_from(duration_ms(PAGE_REFRESH_AFTER)).unwrap_or(0));
    Ok(conn
        .prepare_cached(
            "SELECT max(coalesce(profile_fetched_at, 0), coalesce(profile_tried_at, 0)) < ?2
             FROM peers WHERE service_id = ?1",
        )?
        .query_row((own, own_stale), |r| r.get(0))?)
}

/// Records that a fetch of `service_id` ended at `now`.
fn set_tried(conn: &Connection, service_id: &str, now: i64) -> Result<()> {
    conn.prepare_cached("UPDATE peers SET profile_tried_at = ?2 WHERE service_id = ?1")?
        .execute((service_id, now))?;
    Ok(())
}

async fn fetch_one<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(&job.key) else {
        return Ok(Done::Finished);
    };
    let own = aci == account.aci;
    let sid = job.key.clone();
    let mut known = shared.db.read(|c| known(c, &sid))?;
    if own {
        // Our own profile: our profile key, the authenticated socket (no access key of ours),
        // no picture (the account row shows it).
        known.profile_key = Some(account.profile_key.clone());
        known.sealed = SealedSenderState::Disabled;
        known.access_key = None;
        known.own = true;
    }
    let response = fetch(shared, socket, aci, &known).await?;
    match response.status {
        StatusCode::NOT_FOUND => {
            let now = shared.now();
            // Never our own row: an account that exists is registered (P1 F6).
            shared.db.zone(|tx| not_found(tx, &sid, own, now))?;
            return Ok(Done::Finished);
        }
        // A rate limit holds every profile fetch until its `Retry-After` (`jobs::settle`).
        StatusCode::PAYLOAD_TOO_LARGE | StatusCode::TOO_MANY_REQUESTS => {
            return Err(Error::RateLimited {
                retry_after_secs: response
                    .headers
                    .get(http::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse().ok()),
            });
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
    if known.own {
        return None;
    }
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
        .filter(|_| !known.own)
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
    if known.own {
        // The row carries our fetch time (the throttle); nothing else of it is ours to change.
        peers::ensure(tx, &sid, now)?;
    } else {
        let access_key = peers::load(tx, &sid)?.access_key;
        peers::set_sealed_state(tx, &sid, sealed_state(body, access_key.as_ref()), now)?;
    }

    let mut patch = ContactPatch::new(user);
    if let Some(key) = &known.profile_key {
        if let Some(name) = body.name.as_deref().and_then(|n| decrypt_name(key, n)) {
            patch.profile_name = Patch::Set(name);
        }
        // The about text and its emoji have fields of their own since core.db 18; a profile
        // without them clears them. The older blob (`Contact.meta`, v1) is dropped here.
        patch.about = match body.about.as_deref().and_then(|a| decrypt_text(key, a)) {
            Some(text) => Patch::Set(text),
            None => Patch::Clear,
        };
        patch.about_emoji = match body
            .about_emoji
            .as_deref()
            .and_then(|a| decrypt_text(key, a))
        {
            Some(text) => Patch::Set(text),
            None => Patch::Clear,
        };
        patch.meta = Patch::Clear;
        // `phoneNumberSharing` (Desktop `profiles.preload.ts`): exactly one byte 0x01 is "shares",
        // anything else decrypted is "does not", and no field is "not sharing" too.
        let sharing = body
            .phone_number_sharing
            .as_deref()
            .and_then(|f| decrypt_field(key, f))
            .map(|bytes| field_says_sharing(&bytes));
        if !known.own {
            peers::set_phone_sharing(tx, &sid, sharing.or(Some(false)), now)?;
        }
    }
    if !known.own {
        // The verdict follows every fetch (the rule's inputs may all have changed with it).
        patch.number_shown = Some(peers::number_shown(tx, &sid)?);
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
    if !known.own {
        peers::mark_registered(tx, &sid, now)?;
    }
    tx.prepare_cached(
        "UPDATE peers SET profile_fetched_at = ?2, profile_tried_at = NULL WHERE service_id = ?1",
    )?
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
    // A person whose last attempt ended without a profile (a 404, an error) is not asked again
    // before a day has passed, so the hourly pass does not knock for the same people for ever
    // (`peers.profile_tried_at`, P1 F6; re-check N5).
    let tried_before =
        now.saturating_sub(i64::try_from(duration_ms(ROUTINE_RETRY_AFTER)).unwrap_or(0));
    let own = ServiceId::from(own).service_id_string();
    let queued = shared.db.zone(|tx| -> Result<usize> {
        let due: Vec<String> = tx
            .prepare_cached(
                "SELECT service_id FROM peers
                 WHERE profile_key IS NOT NULL AND service_id != ?1
                   AND (profile_fetched_at IS NULL OR profile_fetched_at < ?2)
                   AND (profile_tried_at IS NULL OR profile_tried_at < ?4)
                 ORDER BY coalesce(profile_fetched_at, 0) LIMIT ?3",
            )?
            .query_map((&own, stale_before, REFRESH_BATCH, tried_before), |r| {
                r.get(0)
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for sid in &due {
            queue(tx, sid, now)?;
        }
        // Our own profile (BQ3), once a day: his own page shows his about text.
        peers::ensure(tx, &own, now)?;
        let own_due = own_fetch_due(tx, &own, now)?;
        if own_due {
            queue(tx, &own, now)?;
        }
        Ok(due.len() + usize::from(own_due))
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

    /// Desktop's reading of `phoneNumberSharing`: only exactly `[0x01]` shares.
    #[test]
    fn only_one_byte_of_one_means_the_number_is_shared() {
        assert!(field_says_sharing(&[1]));
        for no in [&[][..], &[0], &[2], &[0xff], &[1, 0], &[0, 1], &[1, 1]] {
            assert!(!field_says_sharing(no), "{no:?}");
        }
    }

    fn db() -> (tempfile::TempDir, kourier_db::ZoneDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([8; 32]),
        )
        .unwrap();
        (dir, db)
    }

    fn queued(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM jobs WHERE kind = 'profile_fetch'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// A person's page asks for a fetch when it opens (P1, his "no mass fetch" rule): at most one
    /// a day per person, none without a profile key to decrypt it with, and our own profile
    /// uses the account's key.
    #[test]
    fn a_page_asks_for_a_profile_at_most_once_a_day() {
        const DAY: i64 = 24 * 3600 * 1000;
        let (_dir, db) = db();
        let aci = Aci::from(uuid::Uuid::from_u128(7));
        let sid = ServiceId::from(aci).service_id_string();
        db.zone(|tx| -> Result<()> {
            // No row, no key: nothing to decrypt with.
            assert!(!request_throttled(tx, &sid, false, 1_000)?);
            peers::ensure(tx, &sid, 1_000)?;
            assert!(
                !request_throttled(tx, &sid, false, 1_000)?,
                "a row without a key"
            );
            assert_eq!(queued(tx), 0);

            assert!(peers::store_profile_key(tx, aci, &[2; 32], 1_000)?);
            // Never fetched: due.
            assert!(request_throttled(tx, &sid, false, 1_000)?);
            assert_eq!(queued(tx), 1);
            tx.execute("DELETE FROM jobs", [])?;

            // Fetched just now: asked twice in a row is zero requests.
            tx.execute(
                "UPDATE peers SET profile_fetched_at = ?2 WHERE service_id = ?1",
                (&sid, 10 * DAY),
            )?;
            assert!(!request_throttled(tx, &sid, false, 10 * DAY + 1)?);
            assert!(!request_throttled(tx, &sid, false, 10 * DAY + DAY - 1)?);
            assert_eq!(queued(tx), 0);
            // A day later it is due again.
            assert!(request_throttled(tx, &sid, false, 11 * DAY + 1)?);
            assert_eq!(queued(tx), 1);
            tx.execute("DELETE FROM jobs", [])?;
            // A fetch time in the future (a clock set back) does not block for ever.
            assert!(request_throttled(tx, &sid, false, 5 * DAY)?);
            tx.execute("DELETE FROM jobs", [])?;

            // Our own profile: the account holds the key, the peer row does not.
            let own = ServiceId::from(Aci::from(uuid::Uuid::from_u128(9))).service_id_string();
            assert!(!request_throttled(tx, &own, false, 1_000)?);
            assert!(request_throttled(tx, &own, true, 1_000)?);
            Ok(())
        })
        .unwrap();
    }
    /// A 429 hold of the profile service is remembered (P1 F1): a page opened during it (for a
    /// person whose fetch is queued and held, and for one that is not queued), the routine
    /// refresh and a new profile key make nothing due before the hold's end.
    #[test]
    fn nothing_makes_a_profile_fetch_due_during_a_rate_limit_hold() {
        const HOUR: i64 = 3600 * 1000;
        let (_dir, db) = db();
        let ids = [21u128, 22, 23].map(|n| Aci::from(uuid::Uuid::from_u128(n)));
        let sids = ids.map(|a| ServiceId::from(a).service_id_string());
        db.zone(|tx| -> Result<()> {
            for aci in ids {
                assert!(peers::store_profile_key(tx, aci, &[2; 32], 1_000)?);
            }
            queue(tx, &sids[0], 1_000)?;
            // What the job runner does after a 429 with Retry-After of an hour.
            assert_eq!(
                jobs::pause_kind(tx, JobKind::ProfileFetch, 1_000, 1_000 + HOUR)?,
                1
            );
            // A page of the held person, a page of one not queued, the routine queue of a third.
            assert!(request_throttled(tx, &sids[0], false, 2_000)?);
            assert!(request_throttled(tx, &sids[1], false, 2_000)?);
            assert!(queue(tx, &sids[2], 2_000)?);
            assert_eq!(queued(tx), 3);
            assert!(
                jobs::next_due(tx, 1_000 + HOUR - 1, true, false)?.is_none(),
                "a fetch is due during the hold"
            );
            let due = jobs::next_due(tx, 1_000 + HOUR, true, false)?.expect("due at the end");
            assert_eq!(due.kind, JobKind::ProfileFetch);
            Ok(())
        })
        .unwrap();
    }
    /// A transport whose [`ScriptedSocket`] answers every request with its status, or fails it when
    /// there is none: enough to run [`run_fetch`] and [`refresh`] in-process (wiring tests).
    struct Scripted;

    struct ScriptedSocket(Option<StatusCode>);

    impl crate::net::ChatSocket for ScriptedSocket {
        async fn request(
            &self,
            _request: ChatRequest,
        ) -> std::result::Result<ChatResponse, crate::net::TransportError> {
            match self.0 {
                Some(status) => Ok(ChatResponse {
                    status,
                    message: None,
                    headers: http::HeaderMap::new(),
                    body: bytes::Bytes::new(),
                }),
                None => Err(crate::net::TransportError::Disconnected),
            }
        }

        async fn send_multi_recipient(
            &self,
            _send: crate::net::MultiRecipientSend,
        ) -> std::result::Result<crate::net::MultiRecipientOutcome, crate::net::TransportError>
        {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn get_upload_form(
            &self,
            _upload_length: u64,
        ) -> std::result::Result<
            std::result::Result<crate::net::UploadForm, crate::net::UploadTooLarge>,
            crate::net::TransportError,
        > {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn disconnect(&self) {}
    }

    impl SignalTransport for Scripted {
        type Socket = ScriptedSocket;

        async fn connect_auth(
            &self,
            _credentials: &crate::net::AuthCredentials,
        ) -> std::result::Result<
            crate::net::Connected<ScriptedSocket, crate::net::ChatEvent>,
            crate::net::TransportError,
        > {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn connect_unauth(
            &self,
        ) -> std::result::Result<
            crate::net::Connected<ScriptedSocket, crate::net::ChatEvent>,
            crate::net::TransportError,
        > {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn connect_provisioning(
            &self,
        ) -> std::result::Result<
            crate::net::Connected<ScriptedSocket, crate::net::ProvisioningEvent>,
            crate::net::TransportError,
        > {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn https(
            &self,
            _request: crate::net::HttpsRequest,
        ) -> std::result::Result<crate::net::HttpsResponse, crate::net::TransportError> {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn https_download(
            &self,
            _request: crate::net::HttpsRequest,
            _sink: crate::net::DownloadSink,
        ) -> std::result::Result<StatusCode, crate::net::TransportError> {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn https_upload(
            &self,
            _request: crate::net::UploadRequest,
        ) -> std::result::Result<crate::net::HttpsResponse, crate::net::TransportError> {
            Err(crate::net::TransportError::Disconnected)
        }

        fn network_changed(&self) {}
    }

    fn job_for(sid: &str) -> Job {
        Job {
            id: 1,
            kind: JobKind::ProfileFetch,
            key: sid.to_owned(),
            attempts: 0,
            expires_at: None,
            payload: None,
            requests: 0,
        }
    }

    fn tried_at(shared: &Shared<Scripted>, sid: &str) -> Option<i64> {
        shared
            .db
            .read(|c| -> Result<Option<i64>> {
                Ok(c.query_row(
                    "SELECT profile_tried_at FROM peers WHERE service_id = ?1",
                    [sid],
                    |r| r.get(0),
                )?)
            })
            .unwrap()
    }

    /// Re-check F6 wiring: the job runner's `run_fetch` records the attempt of a fetch that
    /// ended without a profile (a transport error, a 404 of another person or of our own row,
    /// a server error) and does not for a rate limit; a 404 marks another person unregistered
    /// and never our own row.
    #[tokio::test]
    async fn run_fetch_records_the_attempt_of_a_fetch_that_ended_without_a_profile() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let other = ServiceId::from(Aci::from(uuid::Uuid::from_u128(41))).service_id_string();
                let fixture = crate::receive::tests::fixture();
                let own = ServiceId::from(fixture.account.aci).service_id_string();
                let outcomes: [(Option<StatusCode>, &str, bool); 5] = [
                    (None, "a transport error", true),
                    (Some(StatusCode::NOT_FOUND), "a 404", true),
                    (Some(StatusCode::INTERNAL_SERVER_ERROR), "a server error", true),
                    (Some(StatusCode::TOO_MANY_REQUESTS), "a rate limit", false),
                    (Some(StatusCode::PAYLOAD_TOO_LARGE), "a 413", false),
                ];
                for sid in [&other, &own] {
                    for (status, what, recorded) in outcomes {
                        let test = crate::actor::testing::shared_over(
                            Arc::new(Scripted),
                            fixture.db.clone(),
                            fixture.account.clone(),
                            Vec::new(),
                        );
                        let shared = Rc::new(test.shared);
                        let _core = test.core;
                        shared
                            .db
                            .zone(|tx| -> Result<()> {
                                peers::ensure(tx, sid, 1)?;
                                tx.execute("UPDATE peers SET profile_tried_at = NULL", [])?;
                                Ok(())
                            })
                            .unwrap();
                        let socket = Arc::new(ScriptedSocket(status));
                        let before = shared.now();
                        let result = run_fetch(&shared, &socket, &job_for(sid)).await;
                        let tried = tried_at(&shared, sid);
                        match (what, status) {
                            ("a 404", _) => assert!(result.is_ok(), "{what}"),
                            _ => assert!(result.is_err(), "{what}"),
                        }
                        assert_eq!(
                            tried.is_some_and(|t| t >= before),
                            recorded,
                            "{what}: the attempt is recorded only when it ended"
                        );
                        if what == "a 404" {
                            let unregistered: bool = shared
                                .db
                                .read(|c| -> Result<bool> {
                                    Ok(c.query_row(
                                        "SELECT unregistered_at IS NOT NULL FROM peers WHERE service_id = ?1",
                                        [sid],
                                        |r| r.get(0),
                                    )?)
                                })
                                .unwrap();
                            assert_eq!(unregistered, sid == &other, "only another person");
                        }
                    }
                }
            })
            .await;
    }

    fn queued_keys(shared: &Shared<Scripted>) -> Vec<String> {
        shared
            .db
            .read(|c| -> Result<Vec<String>> {
                let mut stmt =
                    c.prepare("SELECT key FROM jobs WHERE kind = 'profile_fetch' ORDER BY key")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?)
            })
            .unwrap()
    }

    /// Re-check F6 wiring and N5: the routine `refresh` queues our own profile only when
    /// `own_fetch_due` says so (not within a day of any attempt), and queues no person whose
    /// last attempt ended without a profile less than a day ago.
    #[tokio::test]
    async fn the_routine_refresh_honours_the_attempt_times() {
        const HOUR: i64 = 3600 * 1000;
        tokio::task::LocalSet::new()
            .run_until(async {
                let fixture = crate::receive::tests::fixture();
                let sid = |n: u128| {
                    ServiceId::from(Aci::from(uuid::Uuid::from_u128(n))).service_id_string()
                };
                let own = ServiceId::from(fixture.account.aci).service_id_string();
                let test = crate::actor::testing::shared_over(
                    Arc::new(Scripted),
                    fixture.db.clone(),
                    fixture.account.clone(),
                    Vec::new(),
                );
                let shared = Rc::new(test.shared);
                let _core = test.core;
                let now = shared.now();
                // Our own profile was tried (and failed) an hour ago; four people hold a key and
                // were never fetched: never tried, tried an hour ago, tried 25 hours ago, and a
                // person whose key we hold and was tried never.
                shared
                    .db
                    .zone(|tx| -> Result<()> {
                        peers::ensure(tx, &own, 1)?;
                        tx.execute(
                            "UPDATE peers SET profile_tried_at = ?2 WHERE service_id = ?1",
                            (&own, now - HOUR),
                        )?;
                        for (n, tried) in [
                            (1u128, None),
                            (2, Some(now - HOUR)),
                            (3, Some(now - 25 * HOUR)),
                        ] {
                            assert!(peers::store_profile_key(
                                tx,
                                Aci::from(uuid::Uuid::from_u128(n)),
                                &[2; 32],
                                1
                            )?);
                            tx.execute(
                                "UPDATE peers SET profile_tried_at = ?2 WHERE service_id = ?1",
                                (sid(n), tried),
                            )?;
                        }
                        Ok(())
                    })
                    .unwrap();
                refresh(&shared, fixture.account.aci).unwrap();
                let mut expected = vec![sid(1), sid(3)];
                expected.sort();
                assert_eq!(
                    queued_keys(&shared),
                    expected,
                    "not our own (tried an hour ago), not the person tried an hour ago"
                );
                // Our own, tried 25 hours ago: due.
                shared
                    .db
                    .zone(|tx| -> Result<()> {
                        tx.execute("DELETE FROM jobs", [])?;
                        tx.execute(
                            "UPDATE peers SET profile_tried_at = ?2 WHERE service_id = ?1",
                            (&own, now - 25 * HOUR),
                        )?;
                        Ok(())
                    })
                    .unwrap();
                refresh(&shared, fixture.account.aci).unwrap();
                assert!(
                    queued_keys(&shared).contains(&own),
                    "a day after its attempt"
                );
            })
            .await;
    }

    /// P1 F6: an attempt that ended without a profile is remembered. A page does not ask again
    /// within the hour; our own fetch is due a day after its last attempt, success or not; a 404
    /// never marks our own row unregistered; a rate limit and our own storage errors end no attempt.
    #[test]
    fn a_failed_fetch_is_not_repeated_at_once() {
        const HOUR: i64 = 3600 * 1000;
        const DAY: i64 = 24 * HOUR;
        let (_dir, db) = db();
        let aci = Aci::from(uuid::Uuid::from_u128(31));
        let sid = ServiceId::from(aci).service_id_string();
        let own = ServiceId::from(Aci::from(uuid::Uuid::from_u128(32))).service_id_string();
        let now = 100 * DAY;
        db.zone(|tx| -> Result<()> {
            assert!(peers::store_profile_key(tx, aci, &[2; 32], 1)?);
            // Fetched two days ago, tried (and failed) half an hour ago: not asked at a click.
            tx.execute(
                "UPDATE peers SET profile_fetched_at = ?2, profile_tried_at = ?3 WHERE service_id = ?1",
                (&sid, now - 2 * DAY, now - HOUR / 2),
            )?;
            assert!(!request_throttled(tx, &sid, false, now)?);
            // An hour and a minute after the failure it is asked again.
            assert!(request_throttled(tx, &sid, false, now + HOUR / 2 + 60_000)?);
            tx.execute("DELETE FROM jobs", [])?;

            // Our own fetch: never tried is due; tried an hour ago is not; a day later it is.
            peers::ensure(tx, &own, 1)?;
            assert!(own_fetch_due(tx, &own, now)?);
            set_tried(tx, &own, now - HOUR)?;
            assert!(!own_fetch_due(tx, &own, now)?);
            assert!(own_fetch_due(tx, &own, now + DAY)?);
            // A row of before the column: the last success counts.
            tx.execute(
                "UPDATE peers SET profile_tried_at = NULL, profile_fetched_at = ?2 WHERE service_id = ?1",
                (&own, now - 2 * DAY),
            )?;
            assert!(own_fetch_due(tx, &own, now)?);

            // 404: another person is unregistered, our own row is not; both record the attempt.
            not_found(tx, &sid, false, now)?;
            not_found(tx, &own, true, now)?;
            assert!(peers::load(tx, &sid)?.unregistered);
            assert!(!peers::load(tx, &own)?.unregistered);
            assert!(!own_fetch_due(tx, &own, now + HOUR)?);
            Ok(())
        })
        .unwrap();
        assert!(ends_an_attempt(&Error::Status {
            what: "profile",
            status: 500
        }));
        assert!(!ends_an_attempt(&Error::RateLimited {
            retry_after_secs: None
        }));
        assert!(!ends_an_attempt(&Error::NotLinked));
    }
}
