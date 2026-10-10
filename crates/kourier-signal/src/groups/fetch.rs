//! Fetching a group from the Storage Service (`SD/ts/groups.preload.ts` `getGroupUpdates`,
//! `updateGroupViaLogs`, `updateGroupViaState`; `WebAPI.preload.ts` `getGroupLog`, `getGroup`,
//! `getGroupAvatar`). Network only: the results are inputs of the zone that applies them
//! ([`super::apply`]).
//!
//! Strategy: a group we never fetched gets its full state (`GET /v2/groups`, no system messages
//! for it, §6.8); a known one gets the change logs from its revision on, inclusive, as Desktop
//! asks for them (`GET /v2/groups/logs/<rev>?includeFirstState=true`, so the request never names
//! a revision past the current one; pages of 206 with `Content-Range: versions a-b/c`, the last
//! page with the current state and the send endorsements). The caller drops the change at the
//! known revision itself. 403 on the logs falls back to the state: still a member (joined after
//! the known revision) or not a member any more; 404 is a group that no longer exists; 401 means
//! today's credential was refused (they are fetched again on the next attempt).

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderName, HeaderValue, Method, StatusCode};
use prost::Message as _;

use super::GroupKeys;
use super::credentials;
use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::net::{HttpsHost, HttpsRequest, SignalTransport};
use crate::proto;

/// Most log pages one fetch follows.
const MAX_PAGES: usize = 64;
/// Timeout of one group request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest group avatar accepted (Desktop's limit is far below).
const MAX_AVATAR_BYTES: u64 = crate::avatar::MAX_AVATAR_LEN;

/// What a fetch got from the server.
#[derive(Debug)]
pub(crate) enum Fetched {
    /// The current state and the changes since the known revision (empty for a first fetch).
    Current {
        /// The state at the newest revision.
        state: Box<proto::Group>,
        /// The changes after the known revision, oldest first.
        changes: Vec<proto::GroupChange>,
        /// `GroupSendEndorsementsResponse` of the last page, when the server sent one.
        endorsements: Option<Vec<u8>>,
    },
    /// We are not a member (403).
    NotMember,
    /// The group does not exist (404).
    Gone,
}

/// The `Cached-Send-Endorsements` header: the expiry (seconds) of the endorsements we hold, or 0
/// (the server then sends new ones).
const CACHED_SEND_ENDORSEMENTS: &str = "cached-send-endorsements";

fn request(
    path: String,
    auth: &HeaderValue,
    endorsements_expire_at: Option<i64>,
) -> Result<HttpsRequest> {
    let mut request = HttpsRequest::new(Method::GET, HttpsHost::Storage, path);
    request.timeout = REQUEST_TIMEOUT;
    request
        .headers
        .insert(http::header::AUTHORIZATION, auth.clone());
    request.headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("application/x-protobuf"),
    );
    let cached = endorsements_expire_at.map_or(0, |ms| ms.max(0) / 1000);
    request.headers.insert(
        HeaderName::from_static(CACHED_SEND_ENDORSEMENTS),
        HeaderValue::from(cached),
    );
    Ok(request)
}

/// A non-success answer: 401 forgets the credentials; everything else is a status error.
fn status_error<T: SignalTransport>(shared: &Shared<T>, status: StatusCode) -> Result<Error> {
    if status == StatusCode::UNAUTHORIZED {
        shared.db.zone(|tx| credentials::forget(tx))?;
    }
    Ok(Error::Status {
        what: "group fetch",
        status: status.as_u16(),
    })
}

/// `GET /v2/groups`: the current state.
async fn state<T: SignalTransport>(
    shared: &Shared<T>,
    auth: &HeaderValue,
    endorsements_expire_at: Option<i64>,
) -> Result<Fetched> {
    let response = shared
        .net
        .https(request(
            "/v2/groups".to_owned(),
            auth,
            endorsements_expire_at,
        )?)
        .await?;
    match response.status {
        StatusCode::OK => {
            let body = proto::GroupResponse::decode(&response.body[..])
                .map_err(|_| Error::Decode("group state"))?;
            let state = body.group.ok_or(Error::Decode("group state"))?;
            Ok(Fetched::Current {
                state: Box::new(state),
                changes: Vec::new(),
                endorsements: Some(body.group_send_endorsements_response).filter(|e| !e.is_empty()),
            })
        }
        StatusCode::FORBIDDEN => Ok(Fetched::NotMember),
        StatusCode::NOT_FOUND => Ok(Fetched::Gone),
        status => Err(status_error(shared, status)?),
    }
}

/// `Content-Range: versions <start>-<end>/<current>` of a 206 page: `end`.
fn page_end(response: &crate::net::HttpsResponse) -> Option<u32> {
    let value = response
        .headers
        .get(http::header::CONTENT_RANGE)?
        .to_str()
        .ok()?;
    let range = value.trim().strip_prefix("versions ")?;
    let (span, _current) = range.split_once('/')?;
    let (_start, end) = span.split_once('-')?;
    end.trim().parse().ok()
}

/// Fetches the group of `keys`: its state when `revision` is `None`, else the logs from it on
/// (inclusive: the change that produced `revision` is among them; the caller drops it).
pub(crate) async fn fetch<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
    account: &Account,
    keys: &GroupKeys,
    revision: Option<u32>,
    endorsements_expire_at: Option<i64>,
) -> Result<Fetched> {
    let auth = credentials::authorization(shared, socket, account, keys).await?;
    let Some(known) = revision else {
        return state(shared, &auth, endorsements_expire_at).await;
    };
    // `updateGroupViaLogs`: the range is inclusive and starts at the known revision.
    let mut from = known;
    let mut first_page = true;
    let mut changes = Vec::new();
    let mut last_state = None;
    let mut endorsements = None;
    for _ in 0..MAX_PAGES {
        let path = format!(
            "/v2/groups/logs/{from}?includeFirstState={first_page}&includeLastState=true\
             &maxSupportedChangeEpoch={}",
            super::SUPPORTED_CHANGE_EPOCH
        );
        first_page = false;
        let response = shared
            .net
            .https(request(path, &auth, endorsements_expire_at)?)
            .await?;
        let partial = match response.status {
            StatusCode::OK => false,
            StatusCode::PARTIAL_CONTENT => true,
            // Not a member any more, or the known revision predates our joining: the state
            // tells which.
            StatusCode::FORBIDDEN => return state(shared, &auth, endorsements_expire_at).await,
            StatusCode::NOT_FOUND => return Ok(Fetched::Gone),
            status => return Err(status_error(shared, status)?),
        };
        let page = proto::GroupChanges::decode(&response.body[..])
            .map_err(|_| Error::Decode("group logs"))?;
        for entry in page.group_changes {
            if let Some(change) = entry.group_change {
                changes.push(change);
            }
            if let Some(state) = entry.group_state {
                last_state = Some(state);
            }
        }
        if !page.group_send_endorsements_response.is_empty() {
            endorsements = Some(page.group_send_endorsements_response);
        }
        if !partial {
            break;
        }
        match page_end(&response) {
            Some(end) if end >= from => from = end.saturating_add(1),
            _ => return Err(Error::Decode("group logs page range")),
        }
    }
    let Some(state) = last_state else {
        // A log without any state (a server ignoring `includeFirstState`/`includeLastState`):
        // the state.
        return state(shared, &auth, endorsements_expire_at).await;
    };
    Ok(Fetched::Current {
        state: Box::new(state),
        changes,
        endorsements,
    })
}

/// Downloads and decrypts the avatar at CDN0 `key` (`getGroupAvatar`).
pub(crate) async fn avatar<T: SignalTransport>(
    shared: &Shared<T>,
    keys: &GroupKeys,
    key: &str,
) -> Result<Vec<u8>> {
    if !valid_cdn_key(key) {
        return Err(Error::Decode("group avatar key"));
    }
    let mut request = HttpsRequest::get(HttpsHost::Cdn(0), format!("/{key}"));
    request.timeout = REQUEST_TIMEOUT;
    request.max_body = MAX_AVATAR_BYTES;
    let response = shared.net.https(request).await?;
    if !response.status.is_success() {
        return Err(Error::Status {
            what: "group avatar",
            status: response.status.as_u16(),
        });
    }
    super::decode::avatar(keys, &response.body).ok_or(Error::Decode("group avatar"))
}

/// CDN0 keys are server-generated (`groups/<id>/<file>`): no `..`, no leading slash, a
/// conservative character set.
pub(crate) fn valid_cdn_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 256
        && !key.starts_with('/')
        && key
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'='))
}
