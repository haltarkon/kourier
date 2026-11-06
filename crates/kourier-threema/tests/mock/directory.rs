//! The fake's DIRECTORY (TH6 L2b, `support/mock/directory.rs`): `identity/fetch_bulk` through the fake's HTTPS client
//! (in process, and over the pipes for a child), answered from the world's identity table in libthreema's JSON form
//! (`LT/https/directory.rs`: `ValidIdentity`, mirrored here by the test), and the knobs that fail it, hold it, answer a
//! status or garbage, leave an ID out or give a wrong key (critic T-H3). Every test ends with the violations check.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_threema::{HttpsClient as _, HttpsMethod, HttpsRequest, HttpsResponse, NetError};

use crate::support;
use crate::support::mock::connector::{BLOB_URL, DIRECTORY_URL};
use crate::support::mock::directory::{Asked, DEFAULT_FEATURE_MASK, FETCH_BULK};
use crate::support::mock::pipe;
use crate::support::mock::{MockWorld, world};
use crate::support::piped::{self, Mode};

const PEER: &str = "PEER0001";
const WORKER: &str = "WORKER01";
const NOBODY: &str = "NOBODY01";
const OMITTED: &str = "OMITTED1";

/// One entry of the answer as libthreema reads it (`LT/https/directory.rs` `ValidIdentity`): `identity`, `type` (0 or
/// 1), `publicKey` (standard base64), `state` (0 or 1, absent = 0), `featureMask`. No `Debug`: it holds a key.
struct Entry {
    identity_type: u64,
    public_key: String,
    state: u64,
    feature_mask: u64,
}

/// Whether `body` is a directory answer (`{"identities": [..]}`).
fn is_answer(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|v| v["identities"].is_array())
}

/// A lookup of `ids` as libthreema's `request_identities` writes it (POST, the user agent, `accept`, the JSON body),
/// with `max_body` 64 KiB.
fn lookup(ids: &[&str]) -> HttpsRequest {
    HttpsRequest {
        method: HttpsMethod::Post,
        url: format!("{DIRECTORY_URL}{FETCH_BULK}"),
        headers: vec![
            ("user-agent".to_owned(), "Kourier/0 (test)".to_owned()),
            ("accept".to_owned(), "application/json".to_owned()),
        ],
        body: serde_json::to_vec(&serde_json::json!({ "identities": ids })).expect("json"),
        max_body: Some(64 << 10),
    }
}

/// The answer's entries by identity, read by libthreema's field names.
fn entries(response: &HttpsResponse) -> BTreeMap<String, Entry> {
    let value: serde_json::Value =
        serde_json::from_slice(&response.body).expect("the directory's answer is JSON");
    value["identities"]
        .as_array()
        .expect("an identities array")
        .iter()
        .map(|e| {
            let field = |name: &str| e[name].as_u64().expect("a number field");
            (
                e["identity"].as_str().expect("an identity").to_owned(),
                Entry {
                    identity_type: field("type"),
                    public_key: e["publicKey"].as_str().expect("a key").to_owned(),
                    state: e.get("state").map_or(0, |_| field("state")),
                    feature_mask: field("featureMask"),
                },
            )
        })
        .collect()
}

fn key_of(w: &MockWorld, id: &str) -> String {
    STANDARD.encode(w.identity(id).expect("known").ck.public)
}

/// The identities the world knows are answered with their keys, as regular and active ones, in libthreema's form; an
/// unknown ID and one the knob leaves out are missing (invalid); a Work and an inactive one, a feature mask and a wrong
/// key as the knobs say; an ID asked twice is answered once; each request is recorded.
#[tokio::test]
async fn the_directory_answers_the_identities_the_world_knows_and_leaves_out_the_rest() {
    let w = world(1401);
    for id in [PEER, WORKER, OMITTED] {
        w.add_identity(id);
    }
    let https = w.net().https();
    let response = https
        .execute(lookup(&[PEER, NOBODY, PEER]))
        .await
        .expect("an answer");
    assert_eq!(response.status, 200);
    let found = entries(&response);
    assert_eq!(
        found.keys().collect::<Vec<_>>(),
        vec![PEER],
        "the known one, once"
    );
    let peer = &found[PEER];
    assert_eq!(
        peer.public_key,
        key_of(&w, PEER),
        "the identity table's key"
    );
    assert_eq!(
        (peer.identity_type, peer.state, peer.feature_mask),
        (0, 0, DEFAULT_FEATURE_MASK)
    );

    let wrong = [7u8; 32];
    w.net().directory().knobs(|k| {
        k.omit.insert(OMITTED.to_owned());
        k.work.insert(WORKER.to_owned());
        k.inactive.insert(WORKER.to_owned());
        k.feature_masks.insert(WORKER.to_owned(), 0x0f);
        k.keys.insert(PEER.to_owned(), wrong);
    });
    let response = https
        .execute(lookup(&[WORKER, OMITTED, PEER]))
        .await
        .expect("an answer");
    let found = entries(&response);
    assert_eq!(found.keys().collect::<Vec<_>>(), vec![PEER, WORKER]);
    let worker = &found[WORKER];
    assert_eq!(
        (worker.identity_type, worker.state, worker.feature_mask),
        (1, 1, 0x0f)
    );
    assert_eq!(
        found[PEER].public_key,
        STANDARD.encode(wrong),
        "the knob's key"
    );
    assert_eq!(
        w.net().directory().asked(),
        vec![
            Asked {
                ids: vec![PEER.to_owned(), NOBODY.to_owned(), PEER.to_owned()],
                status: Some(200),
                answered: 1,
            },
            Asked {
                ids: vec![WORKER.to_owned(), OMITTED.to_owned(), PEER.to_owned()],
                status: Some(200),
                answered: 2,
            },
        ]
    );
    assert_eq!(w.net().refused_https(), 0, "nothing refused");
    w.violations().assert_empty();
}

/// T-H3's failures: the directory unreachable (the error), 429 and 503 with an empty body, a 200 that is no answer,
/// and an answer over the request's `max_body` (failed as the real client fails it). Each recorded.
#[tokio::test]
async fn the_directory_fails_or_answers_a_status_or_garbage_as_its_knobs_say() {
    let w = world(1402);
    w.add_identity(PEER);
    let https = w.net().https();
    let d = w.net().directory();
    d.knobs(|k| k.fail = Some(NetError::Failed("https")));
    assert_eq!(
        https.execute(lookup(&[PEER])).await,
        Err(NetError::Failed("https")),
        "unreachable"
    );
    for status in [429, 503] {
        d.knobs(|k| {
            k.fail = None;
            k.status = Some(status);
        });
        let response = https.execute(lookup(&[PEER])).await.expect("an answer");
        assert!(
            response.status == status && response.body.is_empty(),
            "{status} with an empty body"
        );
    }
    d.knobs(|k| {
        k.status = None;
        k.garbage = true;
    });
    let response = https.execute(lookup(&[PEER])).await.expect("an answer");
    assert_eq!(response.status, 200);
    assert!(!is_answer(&response.body), "the body is no answer");
    d.knobs(|k| k.garbage = false);
    let mut small = lookup(&[PEER]);
    small.max_body = Some(16);
    assert_eq!(
        https.execute(small).await,
        Err(NetError::Failed("https")),
        "over max_body"
    );
    let statuses: Vec<Option<u16>> = d.asked().into_iter().map(|a| a.status).collect();
    assert_eq!(
        statuses,
        vec![None, Some(429), Some(503), Some(200), None],
        "each request recorded"
    );
    w.violations().assert_empty();
}

/// THE DELAY: while the hold is up a request pends (counted), a request the client gives up on is dropped unanswered
/// and unrecorded, and one still waiting is served by the knobs of the moment the hold is lifted.
#[tokio::test(start_paused = true)]
async fn a_held_directory_request_pends_until_the_hold_is_lifted() {
    let w = world(1403);
    w.add_identity(PEER);
    let net = w.net().clone();
    net.hold_directory(true);
    let gave_up = tokio::time::timeout(
        Duration::from_secs(15),
        net.https().execute(lookup(&[PEER])),
    )
    .await;
    assert!(gave_up.is_err(), "the client's deadline ended the wait");
    assert_eq!(net.directory_held(), 1);
    let waiting = {
        let net = net.clone();
        tokio::spawn(async move { net.https().execute(lookup(&[PEER])).await })
    };
    assert!(
        net.wait(|| net.directory_held() == 2, Duration::from_secs(5))
            .await,
        "the second request pends"
    );
    assert!(net.directory().asked().is_empty(), "nothing served yet");
    net.directory().knobs(|k| k.status = Some(429));
    net.hold_directory(false);
    let response = waiting.await.expect("the task").expect("an answer");
    assert_eq!(response.status, 429, "the knobs at the lift");
    assert_eq!(
        net.directory().asked().len(),
        1,
        "the dropped one was never served"
    );
    w.violations().assert_empty();
}

/// What the directory does not take: another method, a missing `accept` or user agent, a body that is no lookup (an
/// ID of the wrong form, no IDs, another key), an unknown path (404); any other host stays refused.
#[tokio::test]
async fn a_malformed_lookup_an_unknown_path_and_another_host_are_refused() {
    let w = world(1404);
    let https = w.net().https();
    let mut get = lookup(&[PEER]);
    get.method = HttpsMethod::Get;
    let mut no_accept = lookup(&[PEER]);
    no_accept.headers.retain(|(n, _)| n != "accept");
    let mut no_agent = lookup(&[PEER]);
    no_agent.headers.retain(|(n, _)| n != "user-agent");
    let mut bad_id = lookup(&["peer0001"]);
    bad_id.body = br#"{"identities": ["peer0001"]}"#.to_vec();
    let mut empty = lookup(&[]);
    empty.body = br#"{"identities": []}"#.to_vec();
    let mut other_key = lookup(&[PEER]);
    other_key.body = br#"{"ids": ["PEER0001"]}"#.to_vec();
    for request in [get, no_accept, no_agent, bad_id, empty, other_key] {
        let response = https.execute(request).await.expect("an answer");
        assert_eq!(response.status, 400, "a malformed lookup");
    }
    let mut path = lookup(&[PEER]);
    path.url = format!("{DIRECTORY_URL}identity/{PEER}");
    assert_eq!(https.execute(path).await.expect("an answer").status, 404);
    let mut blob = lookup(&[PEER]);
    blob.url = format!("{BLOB_URL}upload");
    assert_eq!(
        https.execute(blob).await,
        Err(NetError::Refused("https")),
        "another host"
    );
    assert_eq!(w.net().refused_https(), 1);
    assert_eq!(
        w.violations().rules(),
        vec![
            "directory-request-malformed",
            "directory-request-malformed",
            "directory-request-malformed",
            "directory-request-malformed",
            "directory-request-malformed",
            "directory-request-malformed",
            "directory-unknown-path",
            "https-request-the-fake-does-not-serve",
        ]
    );
}

/// CHILD: through the pipes' HTTPS client (what a child actor's parts carry) a lookup reaches the PARENT's directory and
/// its answer comes back whole (the parent's knob key), and a request to another host comes back refused.
#[tokio::test]
async fn directory_child() {
    if !support::offline::in_child() {
        return;
    }
    let pipe = pipe::open_child().await;
    let https = pipe.https();
    let response = https
        .execute(lookup(&[PEER, NOBODY]))
        .await
        .expect("the parent's directory answered");
    assert_eq!(response.status, 200);
    assert!(
        response
            .headers
            .contains(&("content-type".to_owned(), "application/json".to_owned())),
        "the headers came back"
    );
    let found = entries(&response);
    assert_eq!(found.keys().collect::<Vec<_>>(), vec![PEER]);
    assert_eq!(found[PEER].public_key, STANDARD.encode([9u8; 32]));
    let mut blob = lookup(&[PEER]);
    blob.url = format!("{BLOB_URL}upload");
    assert_eq!(https.execute(blob).await, Err(NetError::Refused("https")));
    pipe.finish();
}

/// A child reaches the parent's directory through the pipes (the request whole, the answer whole), and a request to
/// another host is refused in the parent.
#[tokio::test]
async fn a_child_reaches_the_directory_through_the_pipes() {
    let p = piped::parent(1405);
    p.world.add_identity(PEER);
    p.world.net().directory().knobs(|k| {
        k.keys.insert(PEER.to_owned(), [9u8; 32]);
    });
    let Some(run) = p.spawn(module_path!(), "directory_child", Mode::Resume, None) else {
        return;
    };
    let ended = run.wait();
    assert!(
        ended.passed(),
        "the child failed: {ended:?}\n{}",
        ended.stderr
    );
    assert_eq!(
        p.world.net().directory().asked(),
        vec![Asked {
            ids: vec![PEER.to_owned(), NOBODY.to_owned()],
            status: Some(200),
            answered: 1,
        }]
    );
    assert_eq!(p.world.net().refused_https(), 1);
    assert_eq!(
        p.world.violations().rules(),
        vec!["https-request-the-fake-does-not-serve"]
    );
}
