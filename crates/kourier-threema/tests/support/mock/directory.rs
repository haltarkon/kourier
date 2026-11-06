//! TH6 L2b: THE DIRECTORY of the fake (`TP/csp.struct.yml` "Valid Contacts Lookup", `:1308-1330`; the request and the
//! answer as libthreema's `https/directory.rs` writes and reads them, `request_identities` / `handle_identities_result`):
//! `POST https://directory.invalid/identity/fetch_bulk` with the body `{"identities": ["<ID>", ..]}` and the headers
//! `user-agent` and `accept: application/json`, answered `200` with `{"identities": [{"identity", "type", "publicKey",
//! "state", "featureMask"}]}` for the identities the WORLD knows (its identity table, the chat server's); an ID the
//! answer leaves out is invalid (`:1316-1330`). Reached through the parts' `HttpsClient` the way the real connector would
//! be (`connector.rs` `MockHttps`, and over the pipes of `pipe.rs` for a child actor). Every other path of the
//! directory's host is `directory-unknown-path` (answered 404), a request of another form `directory-request-malformed`
//! (answered 400); a request to any other host stays refused (`https-request-the-fake-does-not-serve`).
//!
//! THE KNOBS (critic T-H3: the directory must be able to fail, delay and answer 429), each [U: real server] for what the
//! real directory sends: [`DirectoryKnobs::fail`] (no answer: the error, e.g. `NetError::Failed("https")` for an
//! unreachable host; over the pipes its operation name must be one the pipe knows), [`DirectoryKnobs::status`] (that
//! status with an empty body: 429, 503, 403), [`DirectoryKnobs::garbage`] (200 with a body that is no answer),
//! [`DirectoryKnobs::omit`] (an ID left out: invalid or revoked), [`DirectoryKnobs::keys`] (another key for an ID: a
//! predefined contact with a wrong key), `work`, `inactive` and `feature_masks`; the DELAY is `Net::hold_directory`
//! (a request pends until the hold is lifted, the client's own deadline drops it). A response over the request's
//! `max_body` fails as the real client fails it. Every request is recorded ([`Directory::asked`]). Rule 9 holds: no
//! libthreema code, `serde_json` and `base64` for the wire form. Nothing here prints an ID or a key: `Debug` prints
//! counts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_threema::{HttpsMethod, HttpsRequest, HttpsResponse, NetError};

use super::connector::DIRECTORY_URL;
use super::{Identities, Violations, is_threema_id, lock};

/// The path of the bulk lookup under the directory's URL (`LT/common/config.rs:540-541`).
pub const FETCH_BULK: &str = "identity/fetch_bulk";
/// The feature mask the fake answers for an identity without its own ([I]: any value; the spec's bits are the
/// client's business).
pub const DEFAULT_FEATURE_MASK: u64 = 0xff;

/// The directory's knobs (default: every known identity answered as a regular, active one).
#[derive(Clone, Default)]
pub struct DirectoryKnobs {
    /// Every request fails with this error (no answer at all).
    pub fail: Option<NetError>,
    /// Every request is answered with this status and an empty body.
    pub status: Option<u16>,
    /// Every request is answered 200 with a body that is no answer of the directory.
    pub garbage: bool,
    /// IDs the answer leaves out although the world knows them (invalid, revoked).
    pub omit: BTreeSet<String>,
    /// IDs answered with this key instead of the identity table's (a predefined contact with a wrong key).
    pub keys: BTreeMap<String, [u8; 32]>,
    /// IDs answered as Work identities (`type` 1).
    pub work: BTreeSet<String>,
    /// IDs answered as inactive (`state` 1).
    pub inactive: BTreeSet<String>,
    /// Feature masks per ID ([`DEFAULT_FEATURE_MASK`] otherwise).
    pub feature_masks: BTreeMap<String, u64>,
}

impl std::fmt::Debug for DirectoryKnobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryKnobs")
            .field("fail", &self.fail)
            .field("status", &self.status)
            .field("garbage", &self.garbage)
            .field("omit", &self.omit.len())
            .field("keys", &self.keys.len())
            .finish_non_exhaustive()
    }
}

/// One request the directory took, in order. `Debug` prints the count of IDs and the outcome only.
#[derive(Clone, PartialEq, Eq)]
pub struct Asked {
    /// The IDs asked for, in the request's order (empty for a request that was not a lookup).
    pub ids: Vec<String>,
    /// The status answered; `None`: the request failed (the `fail` knob, or a body over `max_body`).
    pub status: Option<u16>,
    /// How many identities the answer carried.
    pub answered: usize,
}

impl std::fmt::Debug for Asked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Asked({} ids, status {:?}, {} answered)",
            self.ids.len(),
            self.status,
            self.answered
        )
    }
}

struct State {
    knobs: DirectoryKnobs,
    asked: Vec<Asked>,
}

/// The directory of one world: a cloneable handle (the world's `Net` holds one).
#[derive(Clone)]
pub struct Directory {
    identities: Identities,
    violations: Violations,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Directory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Directory")
    }
}

/// Whether `url` names the fake directory's host.
pub fn is_directory(url: &str) -> bool {
    url.starts_with(DIRECTORY_URL)
}

/// The IDs of a lookup's body, or `None` when it is not `{"identities": [<Threema ID>, ..]}` with at least one.
fn lookup_ids(body: &[u8]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let list = object.get("identities")?.as_array()?;
    let ids: Vec<String> = list
        .iter()
        .map(|v| v.as_str().filter(|id| is_threema_id(id)).map(str::to_owned))
        .collect::<Option<_>>()?;
    (!ids.is_empty()).then_some(ids)
}

/// The value of header `name` (any case), if the request has exactly one.
fn header<'a>(request: &'a HttpsRequest, name: &str) -> Option<&'a str> {
    let mut found = request
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name));
    let first = found.next()?;
    found.next().is_none().then_some(first.1.as_str())
}

fn response(status: u16, body: Vec<u8>) -> HttpsResponse {
    HttpsResponse {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body,
    }
}

impl Directory {
    pub fn new(identities: Identities, violations: Violations) -> Self {
        Self {
            identities,
            violations,
            state: Arc::new(Mutex::new(State {
                knobs: DirectoryKnobs::default(),
                asked: Vec::new(),
            })),
        }
    }

    /// Changes the knobs (for the requests that come afterwards).
    pub fn knobs(&self, change: impl FnOnce(&mut DirectoryKnobs)) {
        change(&mut lock(&self.state).knobs);
    }

    /// Every request the directory took, in order.
    pub fn asked(&self) -> Vec<Asked> {
        lock(&self.state).asked.clone()
    }

    /// Serves one request to the directory's host (module doc).
    pub fn serve(&self, request: &HttpsRequest) -> Result<HttpsResponse, NetError> {
        let path = request
            .url
            .strip_prefix(DIRECTORY_URL)
            .unwrap_or(&request.url);
        if path != FETCH_BULK {
            self.violations.record("directory-unknown-path", &[]);
            return self.record(Vec::new(), Ok(response(404, Vec::new())), 0);
        }
        let ids = lookup_ids(&request.body);
        let form_ok = request.method == HttpsMethod::Post
            && header(request, "accept") == Some("application/json")
            && header(request, "user-agent").is_some_and(|ua| !ua.trim().is_empty())
            && ids.is_some();
        let Some(ids) = ids.filter(|_| form_ok) else {
            self.violations.record("directory-request-malformed", &[]);
            return self.record(Vec::new(), Ok(response(400, Vec::new())), 0);
        };
        let knobs = lock(&self.state).knobs.clone();
        if let Some(error) = knobs.fail {
            return self.record(ids, Err(error), 0);
        }
        if let Some(status) = knobs.status {
            return self.record(ids, Ok(response(status, Vec::new())), 0);
        }
        if knobs.garbage {
            return self.record(
                ids,
                Ok(response(200, br#"{"identities": "none"}"#.to_vec())),
                0,
            );
        }
        let table = lock(&self.identities).clone();
        let mut seen = BTreeSet::new();
        let entries: Vec<serde_json::Value> = ids
            .iter()
            .filter(|id| seen.insert(id.as_str()) && !knobs.omit.contains(*id))
            .filter_map(|id| {
                let known = table.get(id)?;
                let key = knobs.keys.get(id).copied().unwrap_or(known.ck.public);
                Some(serde_json::json!({
                    "identity": id,
                    "type": u8::from(knobs.work.contains(id)),
                    "publicKey": STANDARD.encode(key),
                    "state": u8::from(knobs.inactive.contains(id)),
                    "featureMask": knobs.feature_masks.get(id).copied().unwrap_or(DEFAULT_FEATURE_MASK),
                }))
            })
            .collect();
        let answered = entries.len();
        let body = serde_json::to_vec(&serde_json::json!({ "identities": entries }))
            .expect("the fake's answer encodes");
        if request.max_body.is_some_and(|most| body.len() > most) {
            return self.record(ids, Err(NetError::Failed("https")), 0);
        }
        self.record(ids, Ok(response(200, body)), answered)
    }

    fn record(
        &self,
        ids: Vec<String>,
        answer: Result<HttpsResponse, NetError>,
        answered: usize,
    ) -> Result<HttpsResponse, NetError> {
        lock(&self.state).asked.push(Asked {
            ids,
            status: answer.as_ref().ok().map(|r| r.status),
            answered,
        });
        answer
    }
}
