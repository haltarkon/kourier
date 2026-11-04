//! P5a-C6: the incoming `SyncMessage.verified` (BQ-1 (a), critic F2/F4 and Appendix M2): applied
//! for the STORED key only, inserted when there is no row, another key stores nothing and
//! fetches the profile, a move out of state 2 releases, a changed state leaves the quiet
//! "... from another device" notice. Keys and ids are compared in code, never printed.

use kourier_backend_api::BackendNotice;
use kourier_model::SystemEvent;

use super::*;
use crate::receive::KIND_CONTENT;
use crate::receive::dispatch::{Ctx, Outcome, dispatch};
use crate::receive::stage2::Row;
use crate::store::{IDENTITY_DEFAULT, IDENTITY_UNVERIFIED, IDENTITY_VERIFIED};

/// The wall clock of these tests.
const NOW: i64 = T as i64 + 60_000;

fn peer() -> Aci {
    Aci::from(Uuid::from_u128(0x796a_bedb_ca4e_4f18_8803_1fde_5b92_1f9f))
}

fn sid(aci: Aci) -> String {
    ServiceId::Aci(aci).service_id_string()
}

fn row(fixture: &Fixture, device: u32, timestamp: u64, content: &proto::Content) -> Row {
    Row {
        arrival: 7,
        received_at: 100,
        kind: KIND_CONTENT.to_owned(),
        sender: sid(fixture.account.aci),
        sender_device: device,
        timestamp,
        server_timestamp: Some(i64::try_from(timestamp).unwrap() + 5),
        content: Some(content.encode_to_vec()),
        attempts: 0,
        sealed: false,
        decrypt_error: None,
        content_hint: None,
        group_id: None,
        delivered_at: None,
    }
}

/// A `SyncMessage.verified` about `who` (ACI binary) with `key` and `state`.
fn verified(who: Aci, key: &[u8], state: proto::verified::State) -> proto::Content {
    proto::Content {
        content: Some(proto::content::Content::SyncMessage(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Verified(proto::Verified {
                destination_aci_binary: Some(Uuid::from(who).as_bytes().to_vec()),
                identity_key: Some(key.to_vec()),
                state: Some(state.into()),
                null_message: Some(vec![0; 3]),
                ..Default::default()
            })),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// The events of dispatching `row` (features stripped), committed.
fn apply(fixture: &Fixture, row: &Row) -> Vec<BackendEvent> {
    let outcome = fixture
        .db
        .zone(|tx| {
            dispatch(
                tx,
                &Ctx {
                    account: &fixture.account,
                    catch_up: false,
                    now: NOW,
                    long_text: None,
                },
                row,
            )
        })
        .unwrap();
    match outcome {
        Outcome::Handled { events, .. } => without_features(events),
        failed @ Outcome::Failed { .. } => panic!("not handled: {failed:?}"),
    }
}

fn new_key() -> Vec<u8> {
    IdentityKeyPair::generate(&mut rand::rng())
        .identity_key()
        .serialize()
        .to_vec()
}

fn store(fixture: &Fixture, who: Aci, key: &[u8], state: i64) {
    fixture
        .db
        .zone(|tx| -> crate::error::Result<usize> {
            Ok(tx.execute(
                "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
                 VALUES (?1, ?2, ?3, 0, 1)",
                (sid(who), key, state),
            )?)
        })
        .unwrap();
}

/// The stored key and state of `who`.
fn stored(fixture: &Fixture, who: Aci) -> Option<(Vec<u8>, i64)> {
    use kourier_db::rusqlite::OptionalExtension as _;
    fixture
        .db
        .read(|c| -> crate::error::Result<Option<(Vec<u8>, i64)>> {
            Ok(c.query_row(
                "SELECT identity_key, verified FROM identities WHERE service_id = ?1",
                [sid(who)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .unwrap()
}

/// The core has the 1:1 chat with `who`.
fn has_chat(fixture: &Fixture, who: Aci) {
    fixture
        .db
        .zone(|tx| crate::peers::set_features_hash(tx, &sid(who), 7, 1))
        .unwrap();
}

/// A job that waits after an untrusted attempt (P5 M1); returns its id.
fn waiting_job(fixture: &Fixture) -> i64 {
    fixture
        .db
        .zone(|tx| -> crate::error::Result<i64> {
            tx.execute(
                "INSERT INTO jobs(kind, key, priority, next_at, created_at, last_error)
                 VALUES ('profile_fetch', 'waiting', 1, ?1, 0, ?2)",
                (NOW + 3_600_000, crate::jobs::UNTRUSTED_ERROR),
            )?;
            Ok(tx.last_insert_rowid())
        })
        .unwrap()
}

fn next_at(fixture: &Fixture, id: i64) -> i64 {
    fixture
        .db
        .read(|c| -> crate::error::Result<i64> {
            Ok(c.query_row("SELECT next_at FROM jobs WHERE id = ?1", [id], |r| r.get(0))?)
        })
        .unwrap()
}

fn profile_fetch_queued(fixture: &Fixture, who: Aci) -> bool {
    fixture
        .db
        .read(|c| -> crate::error::Result<i64> {
            Ok(c.query_row(
                "SELECT count(*) FROM jobs WHERE kind = 'profile_fetch' AND key = ?1",
                [sid(who)],
                |r| r.get(0),
            )?)
        })
        .unwrap()
        == 1
}

fn notices(events: &[BackendEvent]) -> Vec<&kourier_backend_api::IncomingMessage> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Message(m)
                if matches!(m.body, Body::System(SystemEvent::VerifiedChanged { .. })) =>
            {
                Some(m)
            }
            _ => None,
        })
        .collect()
}

fn released(events: &[BackendEvent], who: &UserId) -> bool {
    events.iter().any(|e| {
        matches!(e, BackendEvent::Notice(BackendNotice::IdentityTrusted { user }) if user == who)
    })
}

fn upsert(events: &[BackendEvent], who: &UserId) -> Option<kourier_model::ContactPatch> {
    events.iter().find_map(|e| match e {
        BackendEvent::ContactUpsert(p) if p.user == *who => Some(p.clone()),
        _ => None,
    })
}

fn user(aci: Aci) -> UserId {
    crate::service_id::aci_user_id(aci).unwrap()
}

/// H1 / M2.2: his phone confirmed the new key (Android sends DEFAULT for the stored key): the
/// row leaves state 2, the core is told to release (`IdentityTrusted`), the jobs waiting on the
/// untrusted key are due, the core gets the trust and key, and the 1:1 chat a quiet notice "from
/// another device" with the id `verified:<envelope timestamp>`, authored by the contact, silent.
#[test]
fn a_default_sync_for_the_stored_key_releases_with_a_notice() {
    use proto::verified::State;
    let fixture = fixture();
    let (ada, key) = (peer(), new_key());
    store(&fixture, ada, &key, IDENTITY_UNVERIFIED);
    has_chat(&fixture, ada);
    let job = waiting_job(&fixture);
    let events = apply(
        &fixture,
        &row(&fixture, 1, T, &verified(ada, &key, State::Default)),
    );
    let (stored_key, state) = stored(&fixture, ada).expect("the row stays");
    assert!(stored_key == key, "the stored key must not change");
    assert!(
        state == IDENTITY_DEFAULT,
        "the state must become DEFAULT, got {state}"
    );
    assert!(
        released(&events, &user(ada)),
        "leaving state 2 must tell the core to release the paused sends"
    );
    assert!(
        next_at(&fixture, job) <= NOW,
        "the waiting job must be due again"
    );
    let patch = upsert(&events, &user(ada)).expect("the contact's upsert");
    assert!(
        patch.trust == Some(TrustLevel::Unverified),
        "trust must be Unverified"
    );
    assert!(
        patch.identity_fingerprint == Patch::Set(IdentityFingerprint(key.clone())),
        "the upsert must carry the stored key"
    );
    let notes = notices(&events);
    assert!(notes.len() == 1, "exactly one notice, got {}", notes.len());
    let note = notes[0];
    assert!(
        &*note.local == format!("verified:{T}").as_str(),
        "the notice id must be verified:<envelope timestamp>"
    );
    assert!(note.silent, "the notice must be silent");
    assert!(
        note.origin
            == Origin::Remote {
                author: user(ada),
                device: None
            },
        "the notice must be authored by the contact"
    );
    assert!(
        note.body
            == Body::System(SystemEvent::VerifiedChanged {
                user: user(ada),
                verified: false,
                other_device: true,
            }),
        "the notice must say 'not verified, from another device'"
    );
}

/// BQ-1 (a): a sync naming ANOTHER key than the stored one stores nothing (a message never
/// replaces a key, and never marks the stored key), tells the core nothing and queues a profile
/// fetch.
#[test]
fn a_sync_for_another_key_stores_nothing_and_fetches_the_profile() {
    use proto::verified::State;
    let fixture = fixture();
    let (ada, ours, theirs) = (peer(), new_key(), new_key());
    store(&fixture, ada, &ours, IDENTITY_DEFAULT);
    has_chat(&fixture, ada);
    let events = apply(
        &fixture,
        &row(&fixture, 1, T, &verified(ada, &theirs, State::Verified)),
    );
    let (stored_key, state) = stored(&fixture, ada).expect("the row stays");
    assert!(
        stored_key == ours,
        "another key must never replace the stored one"
    );
    assert!(
        state == IDENTITY_DEFAULT,
        "the stored key must not be marked, got {state}"
    );
    assert!(
        events.is_empty(),
        "nothing reaches the core, got {} events",
        events.len()
    );
    assert!(
        profile_fetch_queued(&fixture, ada),
        "a profile fetch must be queued"
    );
}

/// M2.1: no row yet: the key from our own account is inserted with its state (better than trust
/// on first use); no notice (there was no state to change) and no release.
#[test]
fn a_sync_without_a_row_inserts_the_key_with_its_state() {
    use proto::verified::State;
    let fixture = fixture();
    let (ada, key) = (peer(), new_key());
    has_chat(&fixture, ada);
    let events = apply(
        &fixture,
        &row(&fixture, 1, T, &verified(ada, &key, State::Verified)),
    );
    let (stored_key, state) = stored(&fixture, ada).expect("the key must be inserted");
    assert!(stored_key == key, "the inserted key must be the message's");
    assert!(
        state == IDENTITY_VERIFIED,
        "the state must be VERIFIED, got {state}"
    );
    let patch = upsert(&events, &user(ada)).expect("the contact's upsert");
    assert!(
        patch.trust == Some(TrustLevel::Verified),
        "trust must be Verified"
    );
    assert!(notices(&events).is_empty(), "no notice for a first key");
    assert!(!released(&events, &user(ada)), "nothing to release");
}

/// A Mark on the phone: one notice; the same state again changes nothing and notes nothing;
/// UNVERIFIED (no device sets it by hand) and syncs about ourselves, from another account or
/// from this device are dropped.
#[test]
fn a_mark_notes_once_and_the_other_shapes_are_dropped() {
    use proto::verified::State;
    let fixture = fixture();
    let (ada, key) = (peer(), new_key());
    store(&fixture, ada, &key, IDENTITY_DEFAULT);
    has_chat(&fixture, ada);
    let events = apply(
        &fixture,
        &row(&fixture, 1, T, &verified(ada, &key, State::Verified)),
    );
    assert!(stored(&fixture, ada).unwrap().1 == IDENTITY_VERIFIED);
    let notes = notices(&events);
    assert!(notes.len() == 1, "one notice for the mark");
    assert!(
        notes[0].body
            == Body::System(SystemEvent::VerifiedChanged {
                user: user(ada),
                verified: true,
                other_device: true,
            }),
        "the notice must say 'verified, from another device'"
    );
    assert!(!released(&events, &user(ada)), "0 -> 1 is no release");
    let again = apply(
        &fixture,
        &row(&fixture, 1, T + 1, &verified(ada, &key, State::Verified)),
    );
    assert!(
        notices(&again).is_empty(),
        "the same state again leaves no notice"
    );
    for (n, (device, content)) in [
        (1, verified(ada, &key, State::Unverified)),
        (1, verified(fixture.account.aci, &key, State::Default)),
        (2, verified(ada, &key, State::Default)),
    ]
    .into_iter()
    .enumerate()
    {
        let events = apply(&fixture, &row(&fixture, device, T + 2, &content));
        assert!(events.is_empty(), "shape {n} must be dropped");
        assert!(
            stored(&fixture, ada).unwrap().1 == IDENTITY_VERIFIED,
            "shape {n} must not change the state"
        );
    }
    let mut other = row(&fixture, 1, T + 3, &verified(ada, &key, State::Default));
    other.sender = sid(Aci::from(Uuid::from_u128(0xbad)));
    assert!(apply(&fixture, &other).is_empty(), "from another account");
    assert!(stored(&fixture, ada).unwrap().1 == IDENTITY_VERIFIED);
}

/// Our click here: Mark (`verified`) or Clear of `key` for `who` at `at` ms; queues the
/// `VerifiedSync` job carrying that time.
fn click(fixture: &Fixture, who: Aci, key: &[u8], verified: bool, at: i64) {
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let outcome = fixture
        .db
        .zone(|tx| {
            crate::safety::set_verified(
                tx,
                &fixture.account,
                &user(who),
                verified,
                &IdentityFingerprint(key.to_vec()),
                at,
                u64::try_from(at).unwrap(),
                &wake,
            )
        })
        .unwrap();
    assert!(
        outcome == crate::safety::SetOutcome::Done,
        "the click was taken"
    );
}

/// The state our queued `VerifiedSync` for `who` carries, `None` when no job is queued.
fn queued_sync_state(fixture: &Fixture, who: Aci) -> Option<i64> {
    use kourier_db::rusqlite::OptionalExtension as _;
    let payload: Option<Option<Vec<u8>>> = fixture
        .db
        .read(|c| -> crate::error::Result<Option<Option<Vec<u8>>>> {
            Ok(c.query_row(
                "SELECT payload FROM jobs WHERE kind = 'verified_sync' AND key = ?1",
                [sid(who)],
                |r| r.get(0),
            )
            .optional()?)
        })
        .unwrap();
    payload.map(|p| {
        let value: serde_json::Value =
            serde_json::from_slice(&p.expect("a payload")).expect("json payload");
        value["state"].as_i64().expect("a state")
    })
}

/// The lead's R3 (protocol MEDIUM-1, storage LOW-3), one order: he clicks Mark here at `T + 10 s`
/// and the job waits; the phone's Clear was sent BEFORE his click (envelope `T + 5 s`) or at
/// the same time: the sync is ignored (Kourier stays Verified, our job still tells the phone),
/// no notice, no event.
#[test]
fn an_older_sync_is_ignored_while_our_click_waits() {
    use proto::verified::State;
    let fixture = fixture();
    let (ada, key) = (peer(), new_key());
    store(&fixture, ada, &key, IDENTITY_DEFAULT);
    has_chat(&fixture, ada);
    let click_at = i64::try_from(T).unwrap() + 10_000;
    click(&fixture, ada, &key, true, click_at);
    for older in [T + 5_000, T + 10_000] {
        let events = apply(
            &fixture,
            &row(&fixture, 1, older, &verified(ada, &key, State::Default)),
        );
        assert!(
            events.is_empty(),
            "an older (or simultaneous) sync must change nothing, got {} events",
            events.len()
        );
        assert!(
            stored(&fixture, ada).unwrap().1 == IDENTITY_VERIFIED,
            "our later click must stay"
        );
        assert!(
            queued_sync_state(&fixture, ada) == Some(IDENTITY_VERIFIED),
            "our job must still tell the phone"
        );
    }
}

/// The lead's R3, the other order: the phone's Clear is NEWER than our Mark (`T + 20 s` against
/// `T + 10 s`): it is applied (Default, the quiet "from another device" notice) and our queued
/// job for that person is deleted, so the phone is not told our older state. The lead's RC4:
/// ONE person's job: our click on another person (Bea) waits beside it and survives (a DELETE
/// that lost its `key` term would take every queued Verified sync). The lead's RC8: ONE kind
/// of job: Ada's queued profile fetch (same key, another kind) survives too (a DELETE that
/// lost its `kind` term would take every job of hers).
#[test]
fn a_newer_sync_wins_and_deletes_our_queued_job() {
    use proto::verified::State;
    let fixture = fixture();
    let (ada, key) = (peer(), new_key());
    let (bea, bea_key) = (Aci::from(Uuid::from_u128(0xbea_0001)), new_key());
    store(&fixture, ada, &key, IDENTITY_DEFAULT);
    store(&fixture, bea, &bea_key, IDENTITY_DEFAULT);
    has_chat(&fixture, ada);
    let click_at = i64::try_from(T).unwrap() + 10_000;
    click(&fixture, bea, &bea_key, true, click_at - 1_000);
    click(&fixture, ada, &key, true, click_at);
    assert!(queued_sync_state(&fixture, ada) == Some(IDENTITY_VERIFIED));
    assert!(queued_sync_state(&fixture, bea) == Some(IDENTITY_VERIFIED));
    fixture
        .db
        .zone(|tx| crate::jobs::enqueue(tx, crate::jobs::JobKind::ProfileFetch, &sid(ada), 0, None))
        .unwrap();
    assert!(profile_fetch_queued(&fixture, ada));
    let events = apply(
        &fixture,
        &row(
            &fixture,
            1,
            T + 20_000,
            &verified(ada, &key, State::Default),
        ),
    );
    assert!(
        stored(&fixture, ada).unwrap().1 == IDENTITY_DEFAULT,
        "the phone's later click must be applied"
    );
    assert!(
        notices(&events).len() == 1,
        "the applied change leaves one notice"
    );
    assert!(
        queued_sync_state(&fixture, ada).is_none(),
        "our older click's job must be deleted"
    );
    assert!(
        queued_sync_state(&fixture, bea) == Some(IDENTITY_VERIFIED),
        "another person's queued Verified sync must survive"
    );
    assert!(
        profile_fetch_queued(&fixture, ada),
        "another kind of job for the same person must survive"
    );
}

/// The lead's RC1 (security re-check F1), the offline click: he clicks Mark here while the
/// phone's own Mark and Clear (both OLDER than the click) wait on the server. The job does not
/// run during the catch-up (`JobKind::needs_online`), so the phone's two syncs, read in that
/// catch-up, meet it and are ignored; once online the job runs and tells the phone the click's
/// state (the runner's belt agrees with the stored state): both devices end on the click.
#[test]
fn a_click_waits_for_the_catch_up_and_the_phones_older_syncs_do_not_beat_it() {
    use proto::verified::State;
    let fixture = fixture();
    let (ada, key) = (peer(), new_key());
    store(&fixture, ada, &key, IDENTITY_DEFAULT);
    has_chat(&fixture, ada);
    click(
        &fixture,
        ada,
        &key,
        true,
        i64::try_from(T).unwrap() + 10_000,
    );

    // The catch-up: our job is not handed out while the account drains its queue.
    assert!(
        fixture
            .db
            .read(|c| crate::jobs::next_due(c, NOW, false, false))
            .unwrap()
            .is_none(),
        "the Verified sync must wait for the catch-up"
    );
    for (at, state) in [(T + 2_000, State::Verified), (T + 5_000, State::Default)] {
        let events = apply(&fixture, &row(&fixture, 1, at, &verified(ada, &key, state)));
        assert!(
            events.is_empty(),
            "an older phone sync read in the catch-up changes nothing"
        );
    }
    assert!(
        stored(&fixture, ada).unwrap().1 == IDENTITY_VERIFIED,
        "Kourier ends on the click"
    );

    // Online: the job runs and the phone is told the click's state.
    let job = fixture
        .db
        .read(|c| crate::jobs::next_due(c, NOW, true, false))
        .unwrap()
        .expect("the job is due once online");
    let content = fixture
        .db
        .read(|c| crate::safety::verified_sync_content(c, &job))
        .unwrap()
        .expect("the runner sends it: key and state are still the click's");
    let Some(proto::content::Content::SyncMessage(sync)) = content.content else {
        panic!("not a sync message");
    };
    let Some(proto::sync_message::Content::Verified(told)) = sync.content else {
        panic!("not a Verified sync");
    };
    assert!(
        told.state == Some(State::Verified.into()),
        "the phone is told the click's state, so both devices end on it"
    );
}
