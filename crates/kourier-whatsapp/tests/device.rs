//! `DeviceStore`: one row, the JSON of upstream's `Device`. The row must keep loading at every pin
//! bump (`testdata/device-50a723d4.json`), or the account would have to be linked again.

use std::sync::Arc;

use wacore::libsignal::protocol::{KeyPair, PrivateKey};
use wacore::store::Device;
use wacore::store::traits::DeviceStore;
use wacore_binary::Jid;
use waproto::whatsapp as wa;

use crate::support::temp_store;

/// A key pair from a fixed TEST private key. It protects nothing.
fn test_pair(seed: u8) -> KeyPair {
    let private = PrivateKey::deserialize(&[seed; 32]).expect("a 32-byte private key");
    let public = private.public_key().expect("its public key");
    KeyPair::new(public, private)
}

/// A device with fixed TEST keys and every optional field set, so that a field upstream renames,
/// drops or re-encodes shows in the fixture comparison. Regenerate the fixture with
/// `KOURIER_REGENERATE_FIXTURES=1 cargo nextest run --workspace --exclude kourier-qt -E
/// 'test(=device::the_fixture_device_still_loads)'` after a deliberate change and review the diff.
#[allow(clippy::field_reassign_with_default)]
pub fn fixture_device() -> Device {
    let mut device = Device::new();
    device.pn = Some("15550000001@s.whatsapp.net".parse::<Jid>().expect("pn"));
    device.lid = Some("100000000000001@lid".parse::<Jid>().expect("lid"));
    device.registration_id = 424_242;
    device.noise_key = test_pair(0x11);
    device.identity_key = test_pair(0x22);
    device.signed_pre_key = test_pair(0x33);
    device.signed_pre_key_id = 7;
    device.signed_pre_key_signature = [0x5a; 64];
    device.adv_secret_key = [0x66; 32];
    // The generated protobuf types are non-exhaustive: built by assigning fields.
    let mut identity = wa::ADVSignedDeviceIdentity::default();
    identity.details = Some(vec![1, 2, 3]);
    identity.account_signature_key = Some(vec![4; 32]);
    identity.account_signature = Some(vec![5; 64]);
    identity.device_signature = Some(vec![6; 64]);
    device.account = Some(Arc::new(identity));
    device.push_name = "Test Device".to_owned();
    device.app_version_primary = 2;
    device.app_version_secondary = 3000;
    device.app_version_tertiary = 12;
    device.app_version_last_fetched_ms = 1_700_000_000_000;
    device.edge_routing_info = Some(vec![9, 8, 7]);
    device.props_hash = Some("2:abcdef".to_owned());
    device.next_pre_key_id = 812;
    device.first_unupload_pre_key_id = 800;
    device.server_has_prekeys = true;
    device.nct_salt = Some(vec![0xa1; 16]);
    device.login_counter = 5;
    device.lid_migrated = true;
    device.last_signed_pre_key_rotation_ms = 1_700_000_100_000;
    device.read_receipts_disabled = true;
    device
}

/// The JSON of a device as the store writes it.
fn json(device: &Device) -> serde_json::Value {
    serde_json::to_value(device).expect("a device serializes")
}

#[tokio::test]
async fn the_device_round_trips_through_the_store_with_every_field() {
    let t = temp_store();
    let s = t.store();
    assert!(!s.exists().await.unwrap());
    assert!(
        s.load().await.unwrap().is_none(),
        "nothing before create or save"
    );
    let device = fixture_device();
    s.save(&device).await.unwrap();
    assert!(s.exists().await.unwrap());
    let loaded = s.load().await.unwrap().expect("the saved device");
    assert_eq!(json(&loaded), json(&device));
    // A later save replaces the row.
    let mut later = device.clone();
    later.push_name = "Changed".to_owned();
    later.next_pre_key_id += 1;
    s.save(&later).await.unwrap();
    assert_eq!(json(&s.load().await.unwrap().unwrap()), json(&later));
    t.owner.close();
}

#[tokio::test]
async fn create_makes_the_one_device_and_a_second_call_keeps_it() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.create().await.unwrap(), 1);
    assert!(s.exists().await.unwrap());
    let first = json(&s.load().await.unwrap().expect("the created device"));
    assert_eq!(s.create().await.unwrap(), 1);
    let second = json(&s.load().await.unwrap().expect("the same device"));
    assert_eq!(first, second, "the second create kept the keys");
    // A save after a create still has one row.
    s.save(&fixture_device()).await.unwrap();
    assert_eq!(s.create().await.unwrap(), 1);
    assert_eq!(
        json(&s.load().await.unwrap().unwrap()),
        json(&fixture_device())
    );
    t.owner.close();
}

/// The committed fixture is a device written by the pinned wacore. It must (a) load, and (b) load
/// into the value it was made from: a field that upstream renamed or dropped changes the value.
/// When this fails after a bump, add a `format = 2` migration in Rust instead of letting the
/// account relink (link and unlink cycles are an account risk).
#[test]
fn the_fixture_device_still_loads() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/device-50a723d4.json");
    if std::env::var_os("KOURIER_REGENERATE_FIXTURES").is_some() {
        let pretty = serde_json::to_string_pretty(&json(&fixture_device())).expect("pretty");
        std::fs::write(&path, pretty + "\n").expect("write the fixture");
    }
    let bytes = std::fs::read(&path).expect("testdata/device-50a723d4.json");
    let loaded: Device = serde_json::from_slice(&bytes).expect("the fixture is a device");
    let stored: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        json(&loaded),
        stored,
        "the fixture does not load into itself"
    );
    assert_eq!(
        json(&loaded),
        json(&fixture_device()),
        "the fixture is out of date"
    );
    assert_eq!(loaded.registration_id, 424_242);
    assert_eq!(loaded.next_pre_key_id, 812);
    assert_eq!(
        loaded.pn.as_ref().map(ToString::to_string).as_deref(),
        Some("15550000001@s.whatsapp.net")
    );
}

/// The record is a blob in the database, not a column per field: the row is `format` 1 and the
/// bytes are the JSON, and nothing of the keys is readable without the database key.
#[tokio::test]
async fn the_device_is_one_json_row_of_format_one() {
    let t = temp_store();
    let s = t.store();
    s.save(&fixture_device()).await.unwrap();
    let conn = kourier_db::open(&t.path(), &crate::support::key()).expect("raw");
    let (rows, format, record): (i64, i64, Vec<u8>) = conn
        .query_row("SELECT count(*), format, record FROM device", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .expect("the row");
    assert_eq!((rows, format), (1, 1));
    let value: serde_json::Value = serde_json::from_slice(&record).expect("JSON");
    assert_eq!(value, json(&fixture_device()));
    // An unknown format is refused, not guessed at.
    conn.execute("UPDATE device SET format = 2", []).unwrap();
    let err = match s.load().await {
        Err(err) => err,
        Ok(_) => panic!("format 2 is not known"),
    };
    assert_eq!(err.to_string(), "serialization/deserialization error");
    t.owner.close();
}

#[tokio::test]
async fn maintenance_and_the_resource_report_work_and_snapshots_stay_off() {
    let t = temp_store();
    let s = t.store();
    s.save(&fixture_device()).await.unwrap();
    s.maintenance().await.expect("a checkpoint");
    let report = s.resource_report().await;
    assert!(report.pages.is_some_and(|p| p > 0), "{report:?}");
    assert!(
        report.memory_bytes.is_some_and(|m| m > 0 && m <= 8_192_000),
        "{report:?}"
    );
    // The trait's default: no second copy of the account's keys is ever written.
    s.snapshot_db("name", Some(b"extra")).await.unwrap();
    let files: Vec<_> = std::fs::read_dir(t.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        files.iter().all(|f| f.starts_with("backend.db")),
        "a snapshot made a file: {files:?}"
    );
    t.owner.close();
}
