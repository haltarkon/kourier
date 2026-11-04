//! TH6a S3: what the harness's `Scene` seeds for the send tests, read back from the file it makes
//! (no actor, so no child): a peer's contact row holds the world identity's REAL client key (a box
//! Kourier seals to it opens at `world.peer(..)`), the members of a group are peers too, the nickname
//! and the settings are the rows the send steps read. A contact made with `with_contacts` keeps the
//! dummy key of the receive tests.

use kourier_db::rusqlite::Connection;
use kourier_threema::store;
use libthreema::protobuf::d2d_sync;
use prost::Message as _;

use super::harness::{ME, scene};
use crate::support::key;

const ANN: &str = "PEER0001";
const BOB: &str = "PEER0002";
const CY: &str = "PEER0003";

fn rows(file: &std::path::Path, sql: &str) -> Vec<Vec<u8>> {
    let db = store::open(file, &key()).expect("open the file");
    db.read(|c: &Connection| {
        let mut statement = c.prepare(sql)?;
        let found = statement
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, kourier_db::Error>(found)
    })
    .expect("read the rows")
}

fn contact_keys(file: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    rows(file, "SELECT sync FROM contact ORDER BY identity")
        .iter()
        .map(|b| {
            let contact = d2d_sync::Contact::decode(b.as_slice()).expect("a stored contact");
            (contact.identity, contact.public_key.expect("a key"))
        })
        .collect()
}

#[test]
fn a_peer_is_a_contact_with_the_world_identitys_real_key() {
    let s = scene(7_001)
        .with_peer(ANN)
        .with_peer(ANN)
        .with_contacts(&[BOB]);
    let ann = s.world.identity(ANN).expect("the peer is in the world");
    assert_eq!(s.world.identity_count(), 2, "KOURIER3 and the peer");
    let dir = tempfile::tempdir().expect("temp dir");
    s.seed_file(dir.path());
    let file = crate::support::db_path(dir.path());
    assert_eq!(
        contact_keys(&file),
        vec![
            (ANN.to_owned(), ann.ck.public.to_vec()),
            (BOB.to_owned(), vec![7; 32])
        ],
        "the peer has the real key once; a plain contact the dummy one"
    );
    s.world.violations().assert_empty();
}

#[test]
fn the_members_of_a_group_are_peers_and_the_group_lists_them() {
    let s = scene(7_002)
        .with_members(ANN, 0x42, &[ANN, BOB])
        .with_members(ME, 0x43, &[CY]);
    let dir = tempfile::tempdir().expect("temp dir");
    s.seed_file(dir.path());
    let file = crate::support::db_path(dir.path());
    let keys: Vec<String> = contact_keys(&file).into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        keys,
        vec![ANN, BOB, CY],
        "every member, not the device itself"
    );
    for (id, key) in contact_keys(&file) {
        let real = s.world.identity(&id).expect("a peer").ck.public;
        assert_eq!(key, real.to_vec(), "a real key");
    }
    let groups: Vec<d2d_sync::Group> = rows(&file, "SELECT sync FROM grp ORDER BY creator")
        .iter()
        .map(|b| d2d_sync::Group::decode(b.as_slice()).expect("a stored group"))
        .collect();
    let described: Vec<(String, u64, Vec<String>)> = groups
        .iter()
        .map(|g| {
            let identity = g.group_identity.as_ref().expect("an identity");
            (
                identity.creator_identity.clone(),
                identity.group_id,
                g.member_identities
                    .as_ref()
                    .map(|m| m.identities.clone())
                    .expect("members"),
            )
        })
        .collect();
    assert_eq!(
        described,
        vec![
            (ME.to_owned(), 0x43, vec![CY.to_owned()]),
            (ANN.to_owned(), 0x42, vec![ANN.to_owned(), BOB.to_owned()]),
        ]
    );
    s.world.violations().assert_empty();
}

#[test]
fn the_nickname_and_the_settings_are_the_rows_the_send_steps_read() {
    // Without them the file has no row of either (a TH1 file, as before).
    let bare = scene(7_003);
    let dir = tempfile::tempdir().expect("temp dir");
    bare.seed_file(dir.path());
    let file = crate::support::db_path(dir.path());
    assert!(rows(&file, "SELECT sync FROM own_profile").is_empty());
    assert!(rows(&file, "SELECT sync FROM settings").is_empty());

    let settings = d2d_sync::Settings {
        read_receipt_policy: Some(d2d_sync::ReadReceiptPolicy::DontSendReadReceipt as i32),
        ..Default::default()
    };
    let s = scene(7_004)
        .with_nickname("Kourier Test")
        .with_settings(settings.clone());
    let dir = tempfile::tempdir().expect("temp dir");
    s.seed_file(dir.path());
    let file = crate::support::db_path(dir.path());
    let profile = rows(&file, "SELECT sync FROM own_profile");
    assert_eq!(profile.len(), 1);
    assert_eq!(
        d2d_sync::UserProfile::decode(profile[0].as_slice())
            .expect("a profile")
            .nickname
            .as_deref(),
        Some("Kourier Test")
    );
    let stored = rows(&file, "SELECT sync FROM settings");
    assert_eq!(stored.len(), 1);
    assert_eq!(
        d2d_sync::Settings::decode(stored[0].as_slice()).expect("settings"),
        settings
    );
    s.world.violations().assert_empty();
}
