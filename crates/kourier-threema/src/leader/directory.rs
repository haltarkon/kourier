//! THE DIRECTORY (TH6 L3's types, L5's call [`lookup`]): what the Valid Contacts Lookup Steps
//! (`TP/csp.struct.yml:1277-1335`) need besides the file: the PREDEFINED CONTACTS with their builtin public keys
//! (`:1127-1185`, production), the body of `POST {directory_url}identity/fetch_bulk` and the reading of its answer
//! (libthreema's form, `LT/https/directory.rs:146-253`; its types are crate-private there), and the per-actor CONTACT
//! LOOKUP CACHE (positive answers only, 30 min, at most 256 IDs [I: the spec names the cache and no lifetime]).
//!
//! An ID the answer leaves out is INVALID (`:1316-1330`); a predefined contact whose answered key is not the builtin one
//! is the spec's exceptional abort (`:1320-1324`), which L5 turns into a close. Nothing here logs: the answer, an ID
//! and a key never reach a log; `Debug` of [`ContactInit`] prints no ID and no key.

use std::collections::HashMap;
use std::fmt;

use base64::Engine as _;

/// The path of the bulk lookup under the directory's base URL (`LT/common/config.rs:540-541`).
pub(crate) const FETCH_BULK: &str = "identity/fetch_bulk";

/// How long a positive answer stays in the cache.
pub(crate) const CACHE_FOR_MS: u64 = 30 * 60 * 1000;

/// How many IDs the cache holds; the oldest goes first.
pub(crate) const CACHE_IDS: usize = 256;

/// The predefined contacts of the pinned spec with their PRODUCTION public keys (`csp.struct.yml:1127-1185`) and
/// whether each is _special_. The same eight IDs as `send::receivers::PREDEFINED_CONTACTS` (a test holds the lists
/// equal); the sandbox keys of `*3MAW0RK` and `*MY3DATA` are not Kourier's (no sandbox build).
pub(crate) const PREDEFINED: [(&str, &str, bool); 8] = [
    (
        "*3MAPUSH",
        "fd711e1a0db0e2f03fcaab6c43da2575b9513664a62a12bd0728d87f7125cc24",
        true,
    ),
    (
        "*3MATOKN",
        "04884d12d668f855d00d71fb1d9d413c95f271312f7e077846af671875c4101b",
        false,
    ),
    (
        "*3MAW0RK",
        "c0e8ad0f50c5c7315c402d3dc26db169408c117613e9b852d3d6c0e87fca536b",
        true,
    ),
    (
        "*3MAWORK",
        "9aa0a72a8fb6f0cc53727fea6096f1b7b0ebefcc2650ad39a1e54837bba0bc4b",
        false,
    ),
    (
        "*BETAFBK",
        "5684d6dcd32a16488df8371095fc9a1fc25baeb6b97366d99fdf2aba00e2bc5c",
        false,
    ),
    (
        "*MY3DATA",
        "3b01854f24736e2d0d2dc387eaf2c0273c5049052147132369bf3960d0a0bf02",
        false,
    ),
    (
        "*SUPPORT",
        "0f944d18324b2132c61d8e40afce60a0ebd701bb11e89be94972d4229e94722a",
        false,
    ),
    (
        "*THREEMA",
        "3a38650c681435bd1fb8498e213a2919b09388f5803aa44640e0f706326a865c",
        false,
    ),
];

/// The nickname of each predefined contact (`csp.struct.yml:1127-1185`), in the order of [`PREDEFINED`] (a test holds both
/// lists to the same IDs). The Valid Contacts Lookup Steps 8.4.3.3.2 (`:1320-1330`) set it, with `FULLY_VERIFIED`, on the
/// contact made from a directory answer; no predefined contact defines a first or a last name.
pub(crate) const PREDEFINED_NICKNAMES: [(&str, &str); 8] = [
    ("*3MAPUSH", "Threema Push"),
    ("*3MATOKN", "Threema Token"),
    ("*3MAW0RK", "Threema Work Delta Sync"),
    ("*3MAWORK", "Threema Work Channel"),
    ("*BETAFBK", "Threema Beta Feedback"),
    ("*MY3DATA", "My Threema Data"),
    ("*SUPPORT", "Threema Support"),
    ("*THREEMA", "Threema Channel"),
];

/// The spec's nickname of a predefined contact, `None` for any other ID (P-L3).
pub(crate) fn predefined_nickname(identity: &str) -> Option<&'static str> {
    PREDEFINED_NICKNAMES
        .iter()
        .find(|(id, _)| *id == identity)
        .map(|(_, nickname)| *nickname)
}

/// The builtin public key of a predefined contact, `None` for any other ID.
pub(crate) fn predefined_key(identity: &str) -> Option<[u8; 32]> {
    let (_, hex, _) = PREDEFINED.iter().find(|(id, _, _)| *id == identity)?;
    let mut key = [0_u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(key)
}

/// Whether `identity` is a _special_ contact (its builtin key is used before a stored contact, note 2).
pub(crate) fn is_special(identity: &str) -> bool {
    PREDEFINED
        .iter()
        .any(|(id, _, special)| *id == identity && *special)
}

/// The properties to create a contact from (`ContactInit` of the spec): what the directory says of a valid ID.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ContactInit {
    /// The Threema ID.
    pub(crate) identity: String,
    /// The public key (32 bytes).
    pub(crate) public_key: [u8; 32],
    /// A Work ID (`type` 1) or not (0).
    pub(crate) work: bool,
    /// Inactive (`state` 1) or active (0, also when the answer leaves it out, as libthreema reads it).
    pub(crate) inactive: bool,
    /// The feature mask.
    pub(crate) feature_mask: u64,
}

impl fmt::Debug for ContactInit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContactInit")
            .field("work", &self.work)
            .field("inactive", &self.inactive)
            .finish_non_exhaustive()
    }
}

/// What the directory says of one asked ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Looked {
    /// A valid ID.
    Valid(ContactInit),
    /// Never existed or revoked: left out of the answer.
    Invalid,
}

/// Why an answer is not used. Carries nothing of the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerError {
    /// Not the JSON form (`identities` an array of objects with the right fields and types).
    Malformed,
    /// A predefined contact's key is not the builtin one (the spec's exceptional abort).
    PredefinedKey,
}

/// The body of the bulk lookup: `{"identities": [..]}` (libthreema's `IdentitiesRequest`).
pub(crate) fn fetch_bulk_body(identities: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "identities": identities })).unwrap_or_default()
}

/// Reads the directory's answer to `asked`: one [`Looked`] per asked ID, in the asked order. An entry for an ID that
/// was not asked is skipped (the spec: log a warning and go on); an asked ID with no entry is [`Looked::Invalid`].
///
/// # Errors
///
/// [`AnswerError::Malformed`] when the JSON is not the form; [`AnswerError::PredefinedKey`] when a predefined contact's
/// key differs from the builtin one.
pub(crate) fn read_answer(
    body: &[u8],
    asked: &[&str],
) -> Result<Vec<(String, Looked)>, AnswerError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| AnswerError::Malformed)?;
    let entries = value
        .get("identities")
        .and_then(serde_json::Value::as_array)
        .ok_or(AnswerError::Malformed)?;
    let mut found: HashMap<String, ContactInit> = HashMap::new();
    for entry in entries {
        let init = entry_of(entry)?;
        if !asked.contains(&init.identity.as_str()) {
            continue;
        }
        if let Some(builtin) = predefined_key(&init.identity)
            && builtin != init.public_key
        {
            return Err(AnswerError::PredefinedKey);
        }
        found.insert(init.identity.clone(), init);
    }
    Ok(asked
        .iter()
        .map(|id| {
            let looked = found.remove(*id).map_or(Looked::Invalid, Looked::Valid);
            ((*id).to_owned(), looked)
        })
        .collect())
}

/// One entry of the answer (libthreema's `ValidIdentity`).
fn entry_of(entry: &serde_json::Value) -> Result<ContactInit, AnswerError> {
    let identity = entry
        .get("identity")
        .and_then(serde_json::Value::as_str)
        .filter(|id| crate::reflect::body::is_identity(id))
        .ok_or(AnswerError::Malformed)?;
    let flag = |name: &str, default: Option<u64>| -> Result<bool, AnswerError> {
        match entry.get(name).map(serde_json::Value::as_u64) {
            None => default.map(|d| d == 1).ok_or(AnswerError::Malformed),
            Some(Some(0)) => Ok(false),
            Some(Some(1)) => Ok(true),
            Some(_) => Err(AnswerError::Malformed),
        }
    };
    let key = entry
        .get("publicKey")
        .and_then(serde_json::Value::as_str)
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok())
        .ok_or(AnswerError::Malformed)?;
    Ok(ContactInit {
        identity: identity.to_owned(),
        public_key: key,
        work: flag("type", None)?,
        inactive: flag("state", Some(0))?,
        feature_mask: entry
            .get("featureMask")
            .and_then(serde_json::Value::as_u64)
            .ok_or(AnswerError::Malformed)?,
    })
}

/// THE CONTACT LOOKUP CACHE, per actor, in memory: positive answers only, each for [`CACHE_FOR_MS`], at most
/// [`CACHE_IDS`] (a new one pushes the oldest out). Lost with the actor (a restart looks up again).
#[derive(Default)]
pub(crate) struct Cache {
    entries: HashMap<String, (ContactInit, u64)>,
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cache")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl Cache {
    /// The cached init of `identity`, if it is younger than [`CACHE_FOR_MS`] at `now_ms`.
    pub(crate) fn get(&self, identity: &str, now_ms: u64) -> Option<&ContactInit> {
        self.entries
            .get(identity)
            .filter(|(_, at)| now_ms.saturating_sub(*at) < CACHE_FOR_MS)
            .map(|(init, _)| init)
    }

    /// Keeps the valid answers of a lookup at `now_ms`; an invalid one is not kept (the next message asks again).
    pub(crate) fn put(&mut self, answers: &[(String, Looked)], now_ms: u64) {
        self.entries
            .retain(|_, (_, at)| now_ms.saturating_sub(*at) < CACHE_FOR_MS);
        for (identity, looked) in answers {
            let Looked::Valid(init) = looked else {
                continue;
            };
            if !self.entries.contains_key(identity) && self.entries.len() >= CACHE_IDS {
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(id, (_, at))| (*at, (*id).clone()))
                    .map(|(id, _)| id.clone());
                if let Some(oldest) = oldest {
                    self.entries.remove(&oldest);
                }
            }
            self.entries
                .insert(identity.clone(), (init.clone(), now_ms));
        }
    }

    /// How many IDs the cache holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

// ---- THE CALL (TH6 L5) ------------------------------------------------------------------------------------------

/// How long one lookup may take on the actor's clock (libthreema's directory `TIMEOUT` is its own client's; this bound
/// is Kourier's [I]): past it the lookup FAILED (critic T-H3: a hang is a failure like the others).
pub(crate) const LOOKUP_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

/// The largest answer taken: one ID asked, a few hundred bytes answered.
const MAX_ANSWER: usize = 64 * 1024;

/// What one lookup of the head's sender came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// The directory answered: what it says of the asked ID.
    Answered(Looked),
    /// The directory could not be asked (critic T-H3): unreachable, a status other than 200 (429 included), an answer
    /// that is not the form, or no answer within [`LOOKUP_WITHIN`]. The message is HELD; the socket's mark is set.
    Failed(&'static str),
    /// A predefined contact's answered key is not the builtin one: the spec's exceptional abort (`:1320-1324`).
    PredefinedKey,
}

/// The failure words (a DEBUG field only; the count line names the hold's rule).
const FAILED_NET: &str = "unreachable";
const FAILED_STATUS: &str = "status";
const FAILED_ANSWER: &str = "answer";
const FAILED_LATE: &str = "late";

/// THE LOOKUP of `identity` (the only HTTPS request of `src/` outside `src/dial/`, rule 16): `POST
/// {directory_url}identity/fetch_bulk` through the parts' client with libthreema's headers (`user-agent` from the
/// CSP client info, `accept: application/json`; `LT/https/directory.rs` `request_identities`), bounded by [`LOOKUP_WITHIN`] on the actor's clock from `asked_at_ms`.
/// Nothing of the request or the answer is logged.
pub(crate) fn lookup(
    https: std::sync::Arc<dyn crate::HttpsClient>,
    directory_url: &str,
    user_agent: String,
    identity: String,
    clock: std::sync::Arc<dyn kourier_backend_api::Clock>,
    asked_at_ms: u64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Fetched>>> {
    let url = format!("{directory_url}{FETCH_BULK}");
    let by =
        asked_at_ms.saturating_add(u64::try_from(LOOKUP_WITHIN.as_millis()).unwrap_or(u64::MAX));
    Box::pin(async move {
        let request = crate::HttpsRequest {
            method: crate::HttpsMethod::Post,
            url,
            headers: vec![
                ("user-agent".to_owned(), user_agent),
                ("accept".to_owned(), "application/json".to_owned()),
            ],
            body: fetch_bulk_body(&[identity.as_str()]),
            max_body: Some(MAX_ANSWER),
        };
        let answer = tokio::select! {
            biased;
            answer = https.execute(request) => answer,
            () = clock.sleep_until(by) => return Fetched::Failed(FAILED_LATE),
        };
        let Ok(response) = answer else {
            return Fetched::Failed(FAILED_NET);
        };
        if response.status != 200 {
            return Fetched::Failed(FAILED_STATUS);
        }
        match read_answer(&response.body, &[identity.as_str()]) {
            Ok(mut answers) => answers
                .pop()
                .map_or(Fetched::Failed(FAILED_ANSWER), |(_, looked)| {
                    Fetched::Answered(looked)
                }),
            Err(AnswerError::Malformed) => Fetched::Failed(FAILED_ANSWER),
            Err(AnswerError::PredefinedKey) => Fetched::PredefinedKey,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANN: &str = "PEER0001";
    const BOB: &str = "PEER0002";

    fn b64(key: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(key)
    }

    /// The predefined list is the send's list, each key is 32 bytes from the spec's hex, and only the two special ones
    /// are special.
    #[test]
    fn the_predefined_contacts_are_the_sends_list_with_the_specs_keys() {
        let ids: Vec<&str> = PREDEFINED.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids, crate::send::receivers::PREDEFINED_CONTACTS);
        for (id, hex, _) in PREDEFINED {
            let key = predefined_key(id).expect("a key");
            let back: String = key.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(back, hex, "{id}");
        }
        assert_eq!(predefined_key(ANN), None);
        let special: Vec<&str> = ids.iter().copied().filter(|id| is_special(id)).collect();
        assert_eq!(special, crate::send::receivers::SPECIAL_CONTACTS);
        // A spot check of one key against the spec's text.
        assert_eq!(predefined_key("*3MAPUSH").expect("push")[..2], [0xfd, 0x71]);
    }

    /// P-L3: the nickname table names the same eight IDs in the same order, and a spot check against the spec's text.
    #[test]
    fn every_predefined_contact_has_the_specs_nickname() {
        let ids: Vec<&str> = PREDEFINED.iter().map(|(id, _, _)| *id).collect();
        let named: Vec<&str> = PREDEFINED_NICKNAMES.iter().map(|(id, _)| *id).collect();
        assert_eq!(named, ids);
        assert_eq!(predefined_nickname("*SUPPORT"), Some("Threema Support"));
        assert_eq!(predefined_nickname("*MY3DATA"), Some("My Threema Data"));
        assert_eq!(predefined_nickname(ANN), None);
        assert!(PREDEFINED_NICKNAMES.iter().all(|(_, n)| !n.is_empty()));
    }

    /// The request body is libthreema's form.
    #[test]
    fn the_request_body_is_one_key_with_the_ids() {
        let body: serde_json::Value =
            serde_json::from_slice(&fetch_bulk_body(&[ANN, BOB])).expect("JSON");
        assert_eq!(body, serde_json::json!({ "identities": [ANN, BOB] }));
        assert_eq!(FETCH_BULK, "identity/fetch_bulk");
    }

    /// The answer: a valid entry with every field, `state` left out (active), an entry not asked (skipped), an asked ID
    /// left out (invalid); the asked order kept.
    #[test]
    fn the_answer_is_read_in_the_asked_order_and_a_missing_id_is_invalid() {
        let body = serde_json::json!({ "identities": [
            { "identity": BOB, "type": 1, "publicKey": b64(&[2; 32]), "state": 1, "featureMask": 255 },
            { "identity": "OTHER001", "type": 0, "publicKey": b64(&[3; 32]), "featureMask": 0 },
        ]});
        let read = read_answer(&serde_json::to_vec(&body).expect("bytes"), &[ANN, BOB])
            .expect("the answer reads");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0], (ANN.to_owned(), Looked::Invalid));
        let Looked::Valid(bob) = &read[1].1 else {
            panic!("BOB is valid");
        };
        assert_eq!(read[1].0, BOB);
        assert_eq!(bob.public_key, [2; 32]);
        assert!(bob.work && bob.inactive);
        assert_eq!(bob.feature_mask, 255);
        assert!(!format!("{bob:?}").contains(BOB), "no ID in Debug");
        let body = serde_json::json!({ "identities": [
            { "identity": ANN, "type": 0, "publicKey": b64(&[4; 32]), "featureMask": 1 },
        ]});
        let read = read_answer(&serde_json::to_vec(&body).expect("bytes"), &[ANN]).expect("reads");
        assert!(matches!(&read[0].1, Looked::Valid(i) if !i.inactive && !i.work));
    }

    /// What is not the form, and a predefined contact with a key that is not the builtin one.
    #[test]
    fn a_malformed_answer_or_a_wrong_predefined_key_is_refused() {
        let bad = |value: serde_json::Value| {
            read_answer(&serde_json::to_vec(&value).expect("bytes"), &[ANN])
        };
        for value in [
            serde_json::json!({}),
            serde_json::json!({ "identities": 1 }),
            serde_json::json!({ "identities": [ { "identity": ANN } ] }),
            serde_json::json!({ "identities": [ { "identity": ANN, "type": 0, "publicKey": b64(&[1; 31]), "featureMask": 0 } ] }),
            serde_json::json!({ "identities": [ { "identity": ANN, "type": 2, "publicKey": b64(&[1; 32]), "featureMask": 0 } ] }),
            serde_json::json!({ "identities": [ { "identity": "short", "type": 0, "publicKey": b64(&[1; 32]), "featureMask": 0 } ] }),
            serde_json::json!({ "identities": [ { "identity": ANN, "type": 0, "publicKey": b64(&[1; 32]) } ] }),
        ] {
            assert_eq!(bad(value), Err(AnswerError::Malformed));
        }
        assert_eq!(
            read_answer(b"not json", &[ANN]),
            Err(AnswerError::Malformed)
        );
        let push = "*3MAPUSH";
        let wrong = serde_json::json!({ "identities": [
            { "identity": push, "type": 0, "publicKey": b64(&[9; 32]), "featureMask": 0 },
        ]});
        assert_eq!(
            read_answer(&serde_json::to_vec(&wrong).expect("bytes"), &[push]),
            Err(AnswerError::PredefinedKey)
        );
        let right = serde_json::json!({ "identities": [
            { "identity": push, "type": 0, "publicKey": b64(&predefined_key(push).expect("k")), "featureMask": 0 },
        ]});
        assert!(read_answer(&serde_json::to_vec(&right).expect("bytes"), &[push]).is_ok());
    }

    /// The cache keeps valid answers for 30 minutes, never an invalid one, and at most 256 IDs (the oldest goes).
    #[test]
    fn the_cache_keeps_valid_answers_for_thirty_minutes_and_at_most_256() {
        let init = |id: &str| ContactInit {
            identity: id.to_owned(),
            public_key: [1; 32],
            work: false,
            inactive: false,
            feature_mask: 0,
        };
        let mut cache = Cache::default();
        cache.put(
            &[
                (ANN.to_owned(), Looked::Valid(init(ANN))),
                (BOB.to_owned(), Looked::Invalid),
            ],
            1_000,
        );
        assert!(cache.get(ANN, 1_000).is_some());
        assert!(cache.get(ANN, 1_000 + CACHE_FOR_MS - 1).is_some());
        assert!(
            cache.get(ANN, 1_000 + CACHE_FOR_MS).is_none(),
            "30 min: gone"
        );
        assert!(
            cache.get(BOB, 1_000).is_none(),
            "an invalid one is not kept"
        );
        for n in 0..CACHE_IDS {
            let id = format!("ID{n:06}");
            cache.put(&[(id.clone(), Looked::Valid(init(&id)))], 2_000 + n as u64);
        }
        assert_eq!(cache.len(), CACHE_IDS);
        assert!(cache.get(ANN, 2_000).is_none(), "the oldest went first");
        assert!(cache.get("ID000000", 3_000).is_some());
        assert!(format!("{cache:?}").contains("256"));
    }
}
