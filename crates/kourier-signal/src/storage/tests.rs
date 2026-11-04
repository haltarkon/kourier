//! Storage Service crypto vectors and merge rules.

use std::collections::BTreeMap;
use std::rc::Rc;

use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_model::{ConvKey, Patch, TrustLevel, UserId};
use libsignal_account_keys::SvrKey;
use libsignal_core::{Aci, Pni, ServiceId};
use tokio::sync::Notify;

use super::*;
use crate::crypto::encrypt_gcm;
use crate::receive::tests::{fixture, without_features};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// libsignal-service-rs's vector (`master_key.rs` `derive_storage_key_from_master_key`): a
/// master key and the storage key Signal Desktop v7.23 derived from it.
#[test]
fn storage_key_matches_desktop() {
    let master = STANDARD
        .decode("9hquLIIZmom8fHF7H8pbUAreawmPLEqli5ceJ94pFkU=")
        .unwrap();
    let storage = STANDARD
        .decode("QMgZ5RGTLFTr4u/J6nypaJX6DKDlSgMw8vmxU6gxnvI=")
        .unwrap();
    let master: [u8; 32] = master.try_into().unwrap();
    assert_eq!(
        &SvrKey::new(master).derive_storage_service_key()[..],
        &storage[..]
    );
}

/// Vectors computed with `@signalapp/mock-server` 27.0.0's `crypto.js` (Signal's own JS
/// implementation of Desktop's `deriveStorageItemKey`, `encryptStorageItem` and
/// `encryptStorageManifest`): storage key 32 × 0x11, record IKM 32 × 0x22, raw id 16 × 0x33,
/// and a `StorageRecord{contact}` for ACI 9d0652a3-…-733f.
mod mock_vectors {
    pub(super) const STORAGE_KEY_FROM_MASTER_55: &str =
        "85bc7ce3f29dfea5a5021a5cc1bf56cfc8c3f8be21848378d6ac11953362d52a";
    pub(super) const MODERN_ITEM_KEY: &str =
        "46ac98a38c8b7544d85e6053850c58c69d060a3ec0c4b9818926191f23365fd5";
    pub(super) const LEGACY_ITEM_KEY: &str =
        "9339869e49490a233960b827c5ed39e3081887bbe6cb93bc248bf1cc63ab8afd";
    pub(super) const MODERN_ITEM: &str = "267f9bbe63ee38661f1051d059842f9af2d21e67f056482595e30a3ceb5ad6878ced3cd9ee5f0d3fb5126cc55e5e47abf4c71ed95bfbf9f0838a02d3a1948b5dc9f89b11315f04a3cf1eafaa427c29599a830bc28e4f10c9ad8050b31737f100b1b5055c6e25315dcd3cc186c40eaca4b0d179974c171fa61a93cf2cadfd68f03efc";
    pub(super) const LEGACY_ITEM: &str = "72f8febf16a15222e1693cb57705d9f9989201570476870a403ab65e23b110bae1baac0d5f5ee2cb36ae6661da227ac4dfcfd88d1c1ae8fafbdf3185a6f3a0fced82baf11088f6d3468bef7156b7169ab3d082062c6ba272dacce93e842b3eea5b9294c6bdc7dcc690eae6422dbc6441fa6e3311ba7cf33cdd8c5f094d6834738fcf";
    pub(super) const MANIFEST_V7: &str = "457b06fe673ebcc17d1e46dbde9af9f09d5f1c880f787a28f83c638c3d11f8ce1255fbf97748de6f2c92eb03f1edf948644faf90b5eb9fc058e3f71839d8f4de9e0be61e72ac1710e171d2bed2f7fd15a4170923da5b";
}

#[test]
fn keys_manifest_and_items_match_signals_implementation() {
    use mock_vectors::*;
    let storage_key = [0x11u8; 32];
    let ikm = [0x22u8; 32];
    let raw_id = [0x33u8; 16];
    assert_eq!(
        &SvrKey::new([0x55; 32]).derive_storage_service_key()[..],
        &hex(STORAGE_KEY_FROM_MASTER_55)[..]
    );
    assert_eq!(
        &item_key(&storage_key, Some(&ikm), &raw_id)[..],
        &hex(MODERN_ITEM_KEY)[..]
    );
    assert_eq!(
        &item_key(&storage_key, None, &raw_id)[..],
        &hex(LEGACY_ITEM_KEY)[..]
    );
    assert_eq!(
        &item_key(&storage_key, Some(&[]), &raw_id)[..],
        &hex(LEGACY_ITEM_KEY)[..],
        "an empty recordIkm means legacy keys"
    );

    let manifest = decrypt_manifest(&storage_key, 7, &hex(MANIFEST_V7)).unwrap();
    assert_eq!(manifest.version, 7);
    assert_eq!(manifest.record_ikm, ikm);
    assert_eq!(manifest.identifiers.len(), 1);
    assert_eq!(manifest.identifiers[0].raw, raw_id);
    assert_eq!(manifest.identifiers[0].r#type, ItemType::Contact as i32);
    assert!(
        decrypt_manifest(&storage_key, 8, &hex(MANIFEST_V7)).is_err(),
        "the version keys it"
    );
    assert!(decrypt_manifest(&[0x12; 32], 7, &hex(MANIFEST_V7)).is_err());

    for (ikm, value) in [(Some(&ikm[..]), MODERN_ITEM), (None, LEGACY_ITEM)] {
        let (record, raw) = decrypt_item(&storage_key, ikm, &raw_id, &hex(value)).unwrap();
        let Some(Record::Contact(contact)) = record.record else {
            panic!("a contact record");
        };
        assert_eq!(hex("9d0652a3dcc34d11975f74d61598733f"), contact.aci_binary);
        assert_eq!(contact.e164, "+15550100");
        assert_eq!(contact.profile_key, vec![0x44; 32]);
        assert_eq!(
            (contact.given_name.as_str(), contact.family_name.as_str()),
            ("Ada", "Lindqvist")
        );
        assert_eq!(contact.system_given_name, "Ada L.");
        assert!(contact.whitelisted && contact.archived && !contact.blocked);
        assert_eq!(contact.muted_until_timestamp, 1_234_567_890_123);
        assert!(!raw.is_empty());
    }
    // A record under the other key scheme does not decrypt.
    assert!(decrypt_item(&storage_key, None, &raw_id, &hex(MODERN_ITEM)).is_err());
    assert!(decrypt_item(&storage_key, Some(&ikm), &[0x34; 16], &hex(MODERN_ITEM)).is_err());
}

// ---------------------------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------------------------

fn item(key: u8, kind: ItemType, record: Record) -> Decrypted {
    let record = proto::StorageRecord {
        record: Some(record),
    };
    Decrypted {
        key: vec![key; 16],
        kind: kind as i32,
        raw: zeroize::Zeroizing::new(record.encode_to_vec()),
        record,
    }
}

fn contact_keyed(key: u8, aci: Aci, edit: impl FnOnce(&mut proto::ContactRecord)) -> Decrypted {
    let mut record = proto::ContactRecord {
        aci_binary: uuid::Uuid::from(aci).as_bytes().to_vec(),
        ..Default::default()
    };
    edit(&mut record);
    item(key, ItemType::Contact, Record::Contact(record))
}

fn outbox(db: &kourier_db::ZoneDb) -> Vec<BackendEvent> {
    let events = db
        .read(|c| kourier_db::outbox::peek(c, 10_000))
        .unwrap()
        .iter()
        .flat_map(|row| decode_outbox_row(&row.envelope).unwrap())
        .collect();
    db.zone(|tx| -> Result<()> {
        tx.execute("DELETE FROM core_outbox", [])?;
        Ok(())
    })
    .unwrap();
    events
}

fn user(aci: Aci) -> UserId {
    aci_user_id(aci).unwrap()
}

fn upserts(events: &[BackendEvent], of: &UserId) -> Vec<kourier_model::ContactPatch> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ContactUpsert(p) if &p.user == of => Some(p.clone()),
            _ => None,
        })
        .collect()
}

fn conversation(events: &[BackendEvent], of: &UserId) -> Option<kourier_model::ConversationPatch> {
    events.iter().find_map(|e| match e {
        BackendEvent::ConversationUpsert(p)
            if p.key == ConvKey::Direct(of.clone()) && p.features.is_none() =>
        {
            Some(p.clone())
        }
        _ => None,
    })
}

#[test]
fn contacts_map_to_contacts_peers_and_chats() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let ada_pni = Pni::from(uuid::Uuid::from_u128(0xada_0001));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let identity = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng());
    let batch = vec![
        contact_keyed(1, ada, |c| {
            c.pni_binary = uuid::Uuid::from(ada_pni).as_bytes().to_vec();
            c.e164 = "+15550111".to_owned();
            c.profile_key = vec![0x41; 32];
            c.given_name = "Ada".to_owned();
            c.family_name = "Lindqvist".to_owned();
            c.system_given_name = "Ada".to_owned();
            c.system_family_name = "L.".to_owned();
            c.whitelisted = true;
            c.archived = true;
            c.muted_until_timestamp = u64::MAX;
            c.identity_key = identity.identity_key().serialize().to_vec();
            c.identity_state = proto::contact_record::IdentityState::Verified as i32;
        }),
        contact_keyed(2, bruno, |c| {
            c.given_name = "Bruno".to_owned();
            c.blocked = true;
        }),
        // Our own ACI is never a contact.
        contact_keyed(3, account.aci, |c| c.given_name = "Me".to_owned()),
        // Neither is a record without any id.
        item(
            9,
            ItemType::Contact,
            Record::Contact(proto::ContactRecord::default()),
        ),
    ];
    let outcome = db
        .zone(|tx| merge_zone(tx, account, &batch, None, true, 100, &wake))
        .unwrap();
    assert!(outcome.profile_fetches, "a new profile key queues a fetch");
    let events = outbox(db);
    let (ada_user, bruno_user) = (user(ada), user(bruno));
    assert!(
        upserts(&events, &user(account.aci)).is_empty(),
        "no own contact"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, BackendEvent::ContactUpsert(_)))
            .count(),
        2
    );
    // ACI + PNI + number join (§3.3), before the contact itself.
    let merged: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::IdentityMerged { from, into } => {
                Some((from.as_str().to_owned(), into.as_str().to_owned()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        merged,
        [
            (
                crate::service_id::pni_user_id(ada_pni)
                    .unwrap()
                    .as_str()
                    .to_owned(),
                ada_user.as_str().to_owned()
            ),
            ("e164:+15550111".to_owned(), ada_user.as_str().to_owned()),
        ]
    );
    let ada_patch = &upserts(&events, &ada_user)[0];
    assert_eq!(
        ada_patch.profile_name,
        Patch::Set("Ada Lindqvist".to_owned())
    );
    assert_eq!(ada_patch.system_name, Patch::Set("Ada L.".to_owned()));
    assert_eq!(ada_patch.blocked, Some(false));
    assert_eq!(ada_patch.trust, Some(TrustLevel::Verified));
    assert!(matches!(ada_patch.identity_fingerprint, Patch::Set(_)));
    assert_eq!(
        ada_patch.identifiers.as_ref().map(|i| i[0].value.as_str()),
        Some("+15550111")
    );
    let ada_chat = conversation(&events, &ada_user).expect("archived and muted: a chat upsert");
    assert_eq!(ada_chat.archived, Some(true));
    assert_eq!(ada_chat.muted_until, Patch::Set(i64::MAX));
    // Its features came with it (accepted: whitelisted).
    assert!(events.iter().any(|e| matches!(e,
        BackendEvent::ConversationUpsert(p)
            if p.key == ConvKey::Direct(ada_user.clone())
                && p.features.as_ref().is_some_and(|f| f.cannot_send.is_none()))));
    let bruno_patch = &upserts(&events, &bruno_user)[0];
    assert_eq!(bruno_patch.blocked, Some(true));
    assert_eq!(bruno_patch.profile_name, Patch::Set("Bruno".to_owned()));
    let bruno_chat = conversation(&events, &bruno_user).expect("blocked: a chat upsert");
    assert_eq!(bruno_chat.blocked, Some(true));

    let sid = |aci: Aci| ServiceId::from(aci).service_id_string();
    db.read(|c| -> Result<()> {
        let peer = peers::load(c, &sid(ada))?;
        assert!(peer.profile_sharing && peer.system_contact && peer.accepted());
        assert_eq!(
            peer.access_key,
            Some(peers::derive_access_key(&[0x41; 32])?)
        );
        assert!(peers::load(c, &sid(bruno))?.blocked);
        assert_eq!(jobs::count(c, JobKind::ProfileFetch)?, 1);
        assert_eq!(
            crate::store::load_identity_key(c, &sid(ada))?,
            Some(*identity.identity_key())
        );
        // Raw records are kept (M2 writes); the version waits for the last zone.
        let stored: i64 = c.query_row("SELECT count(*) FROM storage_records", [], |r| r.get(0))?;
        assert_eq!(stored, 4);
        let version: Option<i64> =
            c.query_row("SELECT manifest_version FROM account", [], |r| r.get(0))?;
        assert_eq!(version, None);
        Ok(())
    })
    .unwrap();

    // The phone changes Ada (a new storage id): unarchived, no longer muted, names unchanged.
    let changed = vec![contact_keyed(0x77, ada, |c| {
        c.pni_binary = uuid::Uuid::from(ada_pni).as_bytes().to_vec();
        c.e164 = "+15550111".to_owned();
        c.profile_key = vec![0x41; 32];
        c.given_name = "Ada".to_owned();
        c.family_name = "Lindqvist".to_owned();
        c.system_given_name = "Ada".to_owned();
        c.system_family_name = "L.".to_owned();
        c.whitelisted = true;
    })];
    let removed = vec![vec![1; 16]];
    db.zone(|tx| {
        merge_zone(
            tx,
            account,
            &changed,
            Some(&LastZone {
                version: 3,
                removed: &removed,
            }),
            false,
            200,
            &wake,
        )
    })
    .unwrap();
    let events = outbox(db);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, BackendEvent::IdentityMerged { .. })),
        "nothing new to join"
    );
    let patch = &upserts(&events, &ada_user)[0];
    assert_eq!(
        patch.system_name,
        Patch::Unchanged,
        "diffed against the previous record"
    );
    let chat = conversation(&events, &ada_user).expect("unarchived");
    assert_eq!(
        (chat.archived, chat.muted_until),
        (Some(false), Patch::Clear)
    );
    db.read(|c| -> Result<()> {
        let (version, synced): (Option<i64>, Option<i64>) = c.query_row(
            "SELECT manifest_version, storage_synced_at FROM account",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!((version, synced), (Some(3), Some(200)));
        let old: i64 = c.query_row(
            "SELECT count(*) FROM storage_records WHERE key = ?1",
            [&removed[0]],
            |r| r.get(0),
        )?;
        assert_eq!(old, 0, "the old storage id is gone");
        Ok(())
    })
    .unwrap();

    // A different identity key in a record waits for a profile fetch instead of replacing ours.
    let other = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng());
    let swapped = vec![contact_keyed(0x78, ada, |c| {
        c.identity_key = other.identity_key().serialize().to_vec();
        c.whitelisted = true;
    })];
    db.zone(|tx| merge_zone(tx, account, &swapped, None, false, 300, &wake))
        .unwrap();
    let events = outbox(db);
    assert_eq!(
        upserts(&events, &ada_user)[0].identity_fingerprint,
        Patch::Unchanged
    );
    db.read(|c| -> Result<()> {
        assert_eq!(
            crate::store::load_identity_key(c, &sid(ada))?,
            Some(*identity.identity_key())
        );
        Ok(())
    })
    .unwrap();
}

/// P5 critic F3: a record's identity state reaches the core's trust as the safety-number row
/// shows it: `UNVERIFIED` (verified once, then the key changed: state 2) is `Changed`,
/// `DEFAULT` is `Unverified`, `VERIFIED` is `Verified`; the stored state follows.
#[test]
fn identity_states_of_records_map_to_the_trust_the_page_shows() {
    use proto::contact_record::IdentityState;
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let people = [
        (0xc01, IdentityState::Unverified, TrustLevel::Changed, 2),
        (0xc02, IdentityState::Default, TrustLevel::Unverified, 0),
        (0xc03, IdentityState::Verified, TrustLevel::Verified, 1),
    ];
    let batch: Vec<Decrypted> = people
        .iter()
        .enumerate()
        .map(|(i, (id, state, _, _))| {
            let key = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng());
            contact_keyed(
                0x40 + u8::try_from(i).unwrap(),
                Aci::from(uuid::Uuid::from_u128(*id)),
                |c| {
                    c.given_name = "P".to_owned();
                    c.whitelisted = true;
                    c.identity_key = key.identity_key().serialize().to_vec();
                    c.identity_state = *state as i32;
                },
            )
        })
        .collect();
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 100, &wake))
        .unwrap();
    let events = outbox(db);
    for (id, _, trust, stored) in people {
        let aci = Aci::from(uuid::Uuid::from_u128(id));
        let patch = &upserts(&events, &user(aci))[0];
        assert!(
            patch.trust == Some(trust),
            "a record's identity state must map to {trust:?}, got {:?}",
            patch.trust
        );
        let state = db
            .read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT verified FROM identities WHERE service_id = ?1",
                    [ServiceId::from(aci).service_id_string()],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(state, stored);
    }
}

/// A number's merge is told once (link feedback L2): a record that comes back after
/// `reset` (or removed and added) tells the core nothing again; a number that moved to
/// another ACI is told; a rebuilt core gets the pair from `links::all`.
#[test]
fn a_numbers_merge_is_told_once_whatever_record_repeats_it() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let numbered = |key: u8, aci: Aci| contact_keyed(key, aci, |c| c.e164 = "+15550111".to_owned());
    let merges = |events: &[BackendEvent]| -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::IdentityMerged { from, into } => {
                    Some((from.as_str().to_owned(), into.as_str().to_owned()))
                }
                _ => None,
            })
            .collect()
    };
    let ada_pair = ("e164:+15550111".to_owned(), user(ada).as_str().to_owned());
    db.zone(|tx| merge_zone(tx, account, &[numbered(1, ada)], None, true, 100, &wake))
        .unwrap();
    assert_eq!(
        merges(&outbox(db)),
        std::slice::from_ref(&ada_pair),
        "the first record tells it"
    );
    // Every record is merged again after a reset (a new storage key): nothing to tell.
    db.zone(|tx| reset(tx, 150)).unwrap();
    db.zone(|tx| merge_zone(tx, account, &[numbered(2, ada)], None, true, 200, &wake))
        .unwrap();
    assert_eq!(merges(&outbox(db)), [], "known: told once");
    // The number went to another account.
    db.zone(|tx| merge_zone(tx, account, &[numbered(3, bruno)], None, false, 300, &wake))
        .unwrap();
    let bruno_pair = ("e164:+15550111".to_owned(), user(bruno).as_str().to_owned());
    assert_eq!(
        merges(&outbox(db)),
        std::slice::from_ref(&bruno_pair),
        "moved: told again"
    );
    let all = db.read(crate::links::all).unwrap();
    assert_eq!(merges(&all), [bruno_pair], "a rebuilt core gets the pair");
}

#[test]
fn quiet_contacts_create_no_chats_and_accepting_updates_known_ones() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let chiara = Aci::from(uuid::Uuid::from_u128(0xc1a));
    let quiet = vec![contact_keyed(1, chiara, |c| {
        c.given_name = "Chiara".to_owned()
    })];
    db.zone(|tx| merge_zone(tx, account, &quiet, None, true, 1, &wake))
        .unwrap();
    let events = outbox(db);
    assert!(
        without_features(events.clone())
            .iter()
            .all(|e| !matches!(e, BackendEvent::ConversationUpsert(_))),
        "a contact without roaming state gets no chat: {events:?}"
    );
    // The core gets the chat from a message (its features are recorded) ...
    let chiara_user = user(chiara);
    db.zone(|tx| crate::features::features_event(tx, account, &chiara_user, 2).map(|_| ()))
        .unwrap();
    // ... and the phone accepts it later: the chat's features change.
    let accepted = vec![contact_keyed(0x55, chiara, |c| {
        c.given_name = "Chiara".to_owned();
        c.whitelisted = true;
    })];
    db.zone(|tx| merge_zone(tx, account, &accepted, None, false, 3, &wake))
        .unwrap();
    let events = outbox(db);
    assert!(events.iter().any(|e| matches!(e,
        BackendEvent::ConversationUpsert(p)
            if p.key == ConvKey::Direct(chiara_user.clone())
                && p.features.as_ref().is_some_and(|f| f.cannot_send.is_none()))));
}

#[test]
fn account_record_maps_settings_note_to_self_and_pins() {
    use proto::account_record::pinned_conversation::{Contact as PinContact, Identifier as Pin};
    use proto::account_record::{PhoneNumberSharingMode, PinnedConversation};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let pins = vec![
        PinnedConversation {
            identifier: Some(Pin::Contact(PinContact {
                service_id_binary: ada.service_id_binary(),
                ..Default::default()
            })),
        },
        PinnedConversation {
            identifier: Some(Pin::GroupMasterKey(vec![7; 32])),
        },
        PinnedConversation {
            identifier: Some(Pin::Contact(PinContact {
                service_id: ServiceId::from(bruno).service_id_string(),
                ..Default::default()
            })),
        },
        PinnedConversation {
            identifier: Some(Pin::Contact(PinContact {
                e164: "+15550199".to_owned(),
                ..Default::default()
            })),
        },
    ];
    let record = proto::AccountRecord {
        profile_key: vec![0x51; 32],
        given_name: "Demo".to_owned(),
        family_name: "Phone".to_owned(),
        read_receipts: true,
        typing_indicators: true,
        link_previews: true,
        phone_number_sharing_mode: PhoneNumberSharingMode::Everybody as i32,
        universal_expire_timer: 3600,
        note_to_self_archived: true,
        pinned_conversations: pins,
        ..Default::default()
    };
    let group = proto::GroupV2Record {
        master_key: vec![7; 32],
        archived: true,
        whitelisted: true,
        ..Default::default()
    };
    let batch = vec![
        item(1, ItemType::Groupv2, Record::GroupV2(group)),
        item(2, ItemType::Account, Record::Account(record)),
    ];
    let outcome = db
        .zone(|tx| merge_zone(tx, account, &batch, None, true, 10, &wake))
        .unwrap();
    assert!(outcome.own_profile_key);
    let events = outbox(db);
    let settings = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::AccountSettings(s) => Some(s.clone()),
            _ => None,
        })
        .expect("settings");
    assert_eq!(settings.read_receipts, Some(true));
    assert_eq!(settings.typing_indicators, Some(true));
    assert_eq!(settings.link_previews, Some(true));
    assert_eq!(
        settings.universal_timer,
        Patch::Set(std::time::Duration::from_secs(3600))
    );
    assert_eq!(settings.share_phone_number, Patch::Set(true));
    let own = user(account.aci);
    assert_eq!(
        upserts(&events, &own)[0].profile_name,
        Patch::Set("Demo Phone".to_owned())
    );
    let note = conversation(&events, &own).expect("Note to Self archived");
    assert_eq!(note.archived, Some(true));
    let pinned = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::PinnedConversations(keys) => Some(keys.clone()),
            _ => None,
        })
        .expect("pins");
    assert_eq!(
        pinned,
        [ConvKey::Direct(user(ada)), ConvKey::Direct(user(bruno))],
        "1:1 pins in order; a group the core does not have and number-only pins are left out"
    );
    // Pinned chats come with their features, before the pin list.
    let pin_at = events
        .iter()
        .position(|e| matches!(e, BackendEvent::PinnedConversations(_)))
        .unwrap();
    for pinned_user in [user(ada), user(bruno)] {
        assert!(events[..pin_at].iter().any(|e| matches!(e,
            BackendEvent::ConversationUpsert(p)
                if p.key == ConvKey::Direct(pinned_user.clone()) && p.features.is_some())));
    }
    db.read(|c| -> Result<()> {
        let settings = crate::db::synced_settings(c)?;
        assert!(settings.read_receipts && settings.typing_indicators);
        assert!(settings.share_number_with_everybody);
        let key: Vec<u8> = c.query_row("SELECT profile_key FROM account", [], |r| r.get(0))?;
        assert_eq!(key, vec![0x51; 32]);
        let groups: BTreeMap<String, i64> = c
            .prepare("SELECT 'archived', archived FROM groups UNION ALL SELECT 'n', count(*) FROM groups")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        assert_eq!(groups.get("n"), Some(&1));
        assert_eq!(groups.get("archived"), Some(&1));
        Ok(())
    })
    .unwrap();

    // The phone prefers address-book photos (`preferContactAvatars`): the setting is read
    // from the stored record, and its change asks the phone for its contacts again.
    assert!(
        !db.read(prefer_contact_avatars).unwrap(),
        "profile pictures first by default"
    );
    let sync_requests = |db: &kourier_db::ZoneDb| -> Vec<String> {
        db.read(|c| -> Result<_> {
            Ok(
                c.prepare("SELECT key FROM jobs WHERE kind = 'sync_request' ORDER BY key")?
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?,
            )
        })
        .unwrap()
    };
    let profile_fetches = |db: &kourier_db::ZoneDb| -> Vec<String> {
        db.read(|c| -> Result<_> {
            Ok(
                c.prepare("SELECT key FROM jobs WHERE kind = 'profile_fetch' ORDER BY key")?
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?,
            )
        })
        .unwrap()
    };
    let before = (sync_requests(db), profile_fetches(db));
    let photos = vec![item(
        4,
        ItemType::Account,
        Record::Account(proto::AccountRecord {
            prefer_contact_avatars: true,
            ..Default::default()
        }),
    )];
    db.zone(|tx| merge_zone(tx, account, &photos, None, false, 15, &wake))
        .unwrap();
    assert!(db.read(prefer_contact_avatars).unwrap());
    assert!(
        sync_requests(db).contains(&"contacts".to_owned()),
        "the address-book photos are asked for"
    );
    assert_eq!(
        profile_fetches(db),
        before.1,
        "no profile is fetched for it"
    );
    // The next version of the record (a new key, as the Storage Service writes one) with the
    // setting unchanged: nothing more is asked.
    db.zone(|tx| -> Result<()> {
        tx.execute("DELETE FROM jobs WHERE kind = 'sync_request'", [])?;
        Ok(())
    })
    .unwrap();
    let again = vec![item(
        5,
        ItemType::Account,
        Record::Account(proto::AccountRecord {
            prefer_contact_avatars: true,
            ..Default::default()
        }),
    )];
    db.zone(|tx| merge_zone(tx, account, &again, None, false, 16, &wake))
        .unwrap();
    assert!(sync_requests(db).is_empty(), "only a change asks");
    // Switched off again: the contacts that show an address-book photo get their profile
    // fetched, the others are left alone.
    let ada_sid = ServiceId::from(ada).service_id_string();
    let bruno_sid = ServiceId::from(bruno).service_id_string();
    db.zone(|tx| -> Result<()> {
        crate::peers::ensure(tx, &ada_sid, 17)?;
        crate::peers::ensure(tx, &bruno_sid, 17)?;
        tx.execute(
            "UPDATE peers SET avatar_blob = 'ab/photo', avatar_source = 'contact_sync'
             WHERE service_id = ?1",
            [&ada_sid],
        )?;
        tx.execute(
            "UPDATE peers SET avatar_blob = 'cd/picture', avatar_source = 'profile'
             WHERE service_id = ?1",
            [&bruno_sid],
        )?;
        tx.execute("DELETE FROM jobs WHERE kind = 'profile_fetch'", [])?;
        Ok(())
    })
    .unwrap();

    // An unset sharing mode is "nobody" (the private reading of UNKNOWN).
    let quiet = vec![item(
        3,
        ItemType::Account,
        Record::Account(proto::AccountRecord::default()),
    )];
    db.zone(|tx| merge_zone(tx, account, &quiet, None, false, 20, &wake))
        .unwrap();
    let events = outbox(db);
    assert!(events.iter().any(|e| matches!(e,
        BackendEvent::AccountSettings(s) if s.share_phone_number == Patch::Set(false))));
    assert!(
        conversation(&events, &own).is_some_and(|c| c.archived == Some(false)),
        "Note to Self unarchived (diffed against the previous account record)"
    );
    assert!(!db.read(prefer_contact_avatars).unwrap());
    assert_eq!(
        profile_fetches(db),
        [ada_sid],
        "profile pictures win again: fetched where a photo shows"
    );
    assert!(sync_requests(db).is_empty());
}

#[test]
fn new_keys_from_the_phone_restart_the_storage_service() {
    let fixture = fixture();
    let db = &fixture.db;
    let pool = "a".repeat(64);
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "INSERT INTO storage_records(key, kind, record, updated_at) VALUES (x'01', 1, x'', 0)",
            [],
        )?;
        tx.execute("UPDATE account SET manifest_version = 9", [])?;
        assert!(apply_keys(tx, Some(&pool), None, 5)?, "a new storage key");
        let records: i64 =
            tx.query_row("SELECT count(*) FROM storage_records", [], |r| r.get(0))?;
        let version: Option<i64> =
            tx.query_row("SELECT manifest_version FROM account", [], |r| r.get(0))?;
        assert_eq!((records, version), (0, None));
        assert_eq!(jobs::count(tx, JobKind::StorageSync)?, 1);
        assert!(
            !apply_keys(tx, Some(&pool), None, 6)?,
            "the same keys again"
        );
        assert!(!apply_keys(tx, Some("not a pool"), None, 7)?);
        assert!(!apply_keys(tx, None, Some(&[1; 3]), 7)?);
        Ok(())
    })
    .unwrap();
}

#[test]
fn gcm_values_of_the_storage_service_decrypt_with_the_shared_helper() {
    let key = manifest_key(&[0x11; 32], 7);
    let manifest = proto::ManifestRecord {
        version: 7,
        ..Default::default()
    };
    let sealed = encrypt_gcm(&key, [4; 12], &manifest.encode_to_vec());
    assert_eq!(
        decrypt_manifest(&[0x11; 32], 7, &sealed).unwrap().version,
        7
    );
}

/// D10 (history import, step S4a): the FIRST record of a contact whose chat the core has
/// states the roaming flags and blocked even where they are false (the archive said true),
/// and so does the first account record for Note to Self. A first record of a contact
/// whose chat the core does not have makes no chat.
#[test]
fn the_first_record_of_a_chat_the_core_has_states_its_flags_whatever_they_are() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    // The core has Ada's chat and Note to Self (an import made them: their features are
    // recorded); it does not have Bruno's.
    for chat in [user(ada), user(account.aci)] {
        db.zone(|tx| crate::features::features_event(tx, account, &chat, 2).map(|_| ()))
            .unwrap();
    }
    let record = proto::AccountRecord {
        given_name: "Demo".to_owned(),
        ..Default::default()
    };
    let batch = vec![
        contact_keyed(1, ada, |c| c.given_name = "Ada".to_owned()),
        contact_keyed(2, bruno, |c| c.given_name = "Bruno".to_owned()),
        item(3, ItemType::Account, Record::Account(record)),
    ];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 10, &wake))
        .unwrap();
    let events = outbox(db);
    let chat = conversation(&events, &user(ada)).expect("Ada's flags are stated");
    assert_eq!(chat.archived, Some(false));
    assert_eq!(chat.marked_unread, Some(false));
    assert_eq!(chat.muted_until, Patch::Clear);
    assert_eq!(chat.blocked, Some(false));
    assert!(
        conversation(&events, &user(bruno)).is_none(),
        "no chat for a contact the core has no chat with"
    );
    let note = conversation(&events, &user(account.aci)).expect("Note to Self's flags");
    assert_eq!(note.archived, Some(false));
    assert_eq!(note.marked_unread, Some(false));

    // The next record of the same contact compares with this one again: nothing changed,
    // nothing is said.
    let again = vec![contact_keyed(4, ada, |c| c.given_name = "Ada".to_owned())];
    db.zone(|tx| merge_zone(tx, account, &again, None, false, 11, &wake))
        .unwrap();
    assert!(conversation(&outbox(db), &user(ada)).is_none());
}

/// Design §3 S4: the pinned list keeps the record's order across kinds, with a group the
/// core has; a group the core does not have is left out (the core would make a bare chat).
#[test]
fn a_group_the_core_has_keeps_its_pin_in_the_records_order() {
    use proto::account_record::PinnedConversation;
    use proto::account_record::pinned_conversation::{Contact as PinContact, Identifier as Pin};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let known = crate::groups::GroupKeys::from_master([7; 32]);
    let unknown = [8u8; 32];
    db.zone(|tx| -> Result<()> {
        crate::groups::ensure(tx, &known, 1)?;
        tx.execute(
            "UPDATE groups SET features_hash = 42 WHERE master_key = ?1",
            [&known.master[..]],
        )?;
        // A group the backend knows and the core does not.
        crate::groups::ensure(tx, &crate::groups::GroupKeys::from_master(unknown), 1)?;
        Ok(())
    })
    .unwrap();
    let pin_group = |master: &[u8]| PinnedConversation {
        identifier: Some(Pin::GroupMasterKey(master.to_vec())),
    };
    let record = proto::AccountRecord {
        pinned_conversations: vec![
            pin_group(&known.master),
            pin_group(&unknown),
            PinnedConversation {
                identifier: Some(Pin::Contact(PinContact {
                    service_id_binary: ada.service_id_binary(),
                    ..Default::default()
                })),
            },
            // Not a master key.
            pin_group(&[9; 16]),
        ],
        ..Default::default()
    };
    let batch = vec![item(1, ItemType::Account, Record::Account(record))];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 10, &wake))
        .unwrap();
    let events = outbox(db);
    let pinned = events
        .iter()
        .find_map(|e| match e {
            BackendEvent::PinnedConversations(keys) => Some(keys.clone()),
            _ => None,
        })
        .expect("pins");
    assert_eq!(pinned, [known.conv_key(), ConvKey::Direct(user(ada))]);
    assert!(
        !events.iter().any(|e| matches!(e,
            BackendEvent::ConversationUpsert(p) if p.key == known.conv_key())),
        "no bare upsert of a group"
    );
}

/// A group the stored account record pins gets the pin list again when its first fetch gives
/// the core its features: the whole list, in the record's order across kinds, naming only
/// chats the core has. A group the record does not pin, and an account without a stored
/// record, give nothing.
#[test]
fn the_first_fetch_of_a_pinned_group_pins_it_from_the_stored_account_record() {
    use proto::account_record::PinnedConversation;
    use proto::account_record::pinned_conversation::{Contact as PinContact, Identifier as Pin};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bob = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let first = crate::groups::GroupKeys::from_master([7; 32]);
    let second = crate::groups::GroupKeys::from_master([8; 32]);
    let unpinned = crate::groups::GroupKeys::from_master([9; 32]);
    let pin_group = |keys: &crate::groups::GroupKeys| PinnedConversation {
        identifier: Some(Pin::GroupMasterKey(keys.master.to_vec())),
    };
    let pin_contact = |aci: Aci| PinnedConversation {
        identifier: Some(Pin::Contact(PinContact {
            service_id_binary: aci.service_id_binary(),
            ..Default::default()
        })),
    };
    let fetched = |keys: &crate::groups::GroupKeys| {
        db.zone(|tx| -> Result<()> {
            crate::groups::ensure(tx, keys, 1)?;
            tx.execute(
                "UPDATE groups SET features_hash = 42 WHERE master_key = ?1",
                [&keys.master[..]],
            )?;
            Ok(())
        })
        .unwrap();
    };
    let list = |keys: &crate::groups::GroupKeys| {
        db.read(|c| pins_with_new_group(c, keys))
            .unwrap()
            .map(|event| match event {
                BackendEvent::PinnedConversations(list) => list,
                other => panic!("{other:?}"),
            })
    };
    // No account record has been merged yet.
    fetched(&first);
    assert_eq!(list(&first), None, "nothing is stored to pin from");

    // The first sync of a fresh link: the groups are not fetched yet, so the pin list the
    // sync sends names the two people only.
    let record = proto::AccountRecord {
        pinned_conversations: vec![
            pin_group(&first),
            pin_contact(ada),
            pin_group(&second),
            pin_contact(bob),
        ],
        ..Default::default()
    };
    db.zone(|tx| {
        tx.execute("DELETE FROM groups", [])?;
        Ok::<_, crate::error::Error>(())
    })
    .unwrap();
    let batch = vec![item(1, ItemType::Account, Record::Account(record))];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 10, &wake))
        .unwrap();
    let sent = outbox(db)
        .iter()
        .find_map(|e| match e {
            BackendEvent::PinnedConversations(keys) => Some(keys.clone()),
            _ => None,
        })
        .expect("pins");
    assert_eq!(
        sent,
        [ConvKey::Direct(user(ada)), ConvKey::Direct(user(bob))]
    );

    // The first group is fetched: the record's whole list, its order across kinds, with the
    // group where the phone put it. The second is not in the core yet and stays out.
    fetched(&first);
    assert_eq!(
        list(&first),
        Some(vec![
            first.conv_key(),
            ConvKey::Direct(user(ada)),
            ConvKey::Direct(user(bob))
        ])
    );
    // Then the second: both groups, still in the record's order.
    fetched(&second);
    assert_eq!(
        list(&second),
        Some(vec![
            first.conv_key(),
            ConvKey::Direct(user(ada)),
            second.conv_key(),
            ConvKey::Direct(user(bob))
        ])
    );
    // A person whose chat the core does not have is not named (nothing makes it here).
    db.zone(|tx| peers::clear_features_hash(tx, &ServiceId::from(bob).service_id_string()))
        .unwrap();
    assert_eq!(
        list(&second),
        Some(vec![
            first.conv_key(),
            ConvKey::Direct(user(ada)),
            second.conv_key()
        ])
    );
    // A group the record does not pin gets nothing.
    fetched(&unpinned);
    assert_eq!(list(&unpinned), None);
}

/// The hook in `groups::apply::apply_left` (S4a, finding 1): a group that the stored account
/// record pins and that the core does not have yet gets the pin list again with its first
/// event, also when that event says we are not in it. This is the case where the backend has
/// the group's state and the core lacks the group (the core's database was lost: `maintenance`
/// clears `features_hash` and keeps the state), so the group is fetched again and is gone.
/// What the code does for a left group is what a group we are in does: the group is named in
/// the record's order, because `groups.features_hash` is set just before the list is built.
/// A group the core already has, a group the record does not pin, and a group that was never
/// fetched (the core never had it: `apply_left` sends nothing) get no list.
#[test]
fn the_first_event_of_a_pinned_group_we_are_not_in_pins_it_from_the_stored_account_record() {
    use crate::groups::apply::{Outcome, Update, apply};
    use crate::groups::{GroupKeys, SnapMember, Snapshot};
    use proto::account_record::PinnedConversation;
    use proto::account_record::pinned_conversation::{Contact as PinContact, Identifier as Pin};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let pinned = GroupKeys::from_master([7; 32]);
    let unpinned = GroupKeys::from_master([8; 32]);
    let had = GroupKeys::from_master([9; 32]);
    let never_fetched = GroupKeys::from_master([10; 32]);
    // The phone pins the group before Ada: a list that put people first would differ.
    let record = proto::AccountRecord {
        pinned_conversations: [&pinned, &had, &never_fetched]
            .into_iter()
            .map(|keys| PinnedConversation {
                identifier: Some(Pin::GroupMasterKey(keys.master.to_vec())),
            })
            .chain([PinnedConversation {
                identifier: Some(Pin::Contact(PinContact {
                    service_id_binary: ada.service_id_binary(),
                    ..Default::default()
                })),
            }])
            .collect(),
        ..Default::default()
    };
    // Ada's chat is in the core (the first sync made it); the groups are not fetched yet, so
    // the sync's list names Ada alone.
    db.zone(|tx| crate::features::features_event(tx, account, &user(ada), 2).map(|_| ()))
        .unwrap();
    let batch = vec![item(1, ItemType::Account, Record::Account(record))];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 10, &wake))
        .unwrap();
    let first_list = outbox(db)
        .iter()
        .find_map(|e| match e {
            BackendEvent::PinnedConversations(keys) => Some(keys.clone()),
            _ => None,
        })
        .expect("the sync's pins");
    assert_eq!(first_list, [ConvKey::Direct(user(ada))]);

    // The backend has the state of each group (we were a member), the core has `had` only.
    let snapshot = Snapshot {
        revision: 3,
        title: "Climbing".to_owned(),
        members: vec![SnapMember {
            aci: uuid::Uuid::from(ada),
            admin: true,
            joined_at: 0,
        }],
        ..Snapshot::default()
    };
    db.zone(|tx| -> Result<()> {
        for (keys, features_hash) in [(&pinned, None), (&unpinned, None), (&had, Some(42i64))] {
            tx.execute(
                "INSERT INTO groups(master_key, group_id, updated_at, revision, state,
                   membership, features_hash) VALUES (?1, ?2, 0, 3, ?3, 'member', ?4)",
                (
                    &keys.master[..],
                    &keys.id[..],
                    snapshot.to_json()?,
                    features_hash,
                ),
            )?;
        }
        // Fetched for the first time and gone: no state, nothing the core ever had.
        crate::groups::ensure(tx, &never_fetched, 1)?;
        Ok(())
    })
    .unwrap();
    let left = |keys: &GroupKeys| {
        db.zone(|tx| -> Result<_> {
            let update = Update {
                keys: *keys,
                outcome: Outcome::NotMember { change: None },
            };
            Ok(apply(tx, account, std::path::Path::new("."), update, None, 20)?.events)
        })
        .unwrap()
    };
    let pins_of = |events: &[BackendEvent]| {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::PinnedConversations(list) => Some(list.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let upserts_of = |events: &[BackendEvent], keys: &GroupKeys| {
        events
            .iter()
            .filter(
                |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == keys.conv_key()),
            )
            .count()
    };

    // The group the record pins: its upsert (we cannot write there), then the whole list in
    // the record's order, the group where the phone put it. The list names the other group
    // the core has and leaves out the one it has not (`never_fetched`).
    let events = left(&pinned);
    assert_eq!(upserts_of(&events, &pinned), 1, "{events:?}");
    let BackendEvent::ConversationUpsert(patch) = &events[0] else {
        panic!("the group's upsert comes first: {events:?}");
    };
    assert_eq!(
        patch.features.as_ref().and_then(|f| f.cannot_send.clone()),
        Some(kourier_model::CannotSend::NotMember)
    );
    assert_eq!(
        pins_of(&events),
        [vec![
            pinned.conv_key(),
            had.conv_key(),
            ConvKey::Direct(user(ada))
        ]],
        "{events:?}"
    );
    assert!(
        matches!(&events[1], BackendEvent::PinnedConversations(_)),
        "the list follows the group's upsert: {events:?}"
    );
    // A group the record does not pin gets its upsert and no list.
    let events = left(&unpinned);
    assert_eq!(upserts_of(&events, &unpinned), 1);
    assert!(pins_of(&events).is_empty(), "{events:?}");
    // A group the core has already keeps the pin the sync gave it: no list again.
    let events = left(&had);
    assert_eq!(upserts_of(&events, &had), 1);
    assert!(pins_of(&events).is_empty(), "{events:?}");
    // A group never fetched before: the core never had it, nothing is sent, not a pin.
    let events = left(&never_fetched);
    assert!(events.is_empty(), "{events:?}");
}

/// The Note to Self half of the first-record rule (`merge_account`): on a fresh link without
/// an import the first account record must not make an empty Note to Self chat, as an upsert
/// would; once the core has the chat (an import made it) the first record states its flags
/// whatever they are, so that the sync wins over the archive (D10).
#[test]
fn a_first_account_record_makes_no_note_to_self_chat_and_states_the_flags_of_one_the_core_has() {
    let record = || proto::AccountRecord {
        given_name: "Demo".to_owned(),
        ..Default::default()
    };
    let note_to_self = |events: &[BackendEvent], account: &crate::db::Account| {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(p)
                    if p.key == ConvKey::Direct(user(account.aci)) =>
                {
                    Some(p.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let wake = Rc::new(Notify::new());

    // A fresh link: the core has no Note to Self chat.
    let fresh = fixture();
    let (db, account) = (&fresh.db, &fresh.account);
    let batch = vec![item(1, ItemType::Account, Record::Account(record()))];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 10, &wake))
        .unwrap();
    let events = outbox(db);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BackendEvent::AccountSettings(_))),
        "the record was merged"
    );
    assert!(
        note_to_self(&events, account).is_empty(),
        "no chat for Note to Self: {events:?}"
    );

    // After an import the core has the chat: the same record states its flags.
    let imported = fixture();
    let (db, account) = (&imported.db, &imported.account);
    db.zone(|tx| crate::features::features_event(tx, account, &user(account.aci), 2).map(|_| ()))
        .unwrap();
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 10, &wake))
        .unwrap();
    let events = outbox(db);
    let stated = note_to_self(&events, account);
    assert_eq!(stated.len(), 1, "{events:?}");
    assert_eq!(stated[0].archived, Some(false));
    assert_eq!(stated[0].marked_unread, Some(false));
}

// ---------------------------------------------------------------------------------------------
// A1 archived chats: the base a record is diffed against, and the setting
// ---------------------------------------------------------------------------------------------

/// The core's chat of `chat` exists (its features were sent), as after an import or a first
/// message: a record of it is then diffed and stated.
fn core_has(fixture: &crate::receive::tests::Fixture, chat: &UserId) {
    fixture
        .db
        .zone(|tx| crate::features::features_event(tx, &fixture.account, chat, 2).map(|_| ()))
        .unwrap();
}

fn roaming_of(db: &kourier_db::ZoneDb, who: Aci) -> Option<peers::Roaming> {
    db.read(|c| peers::load(c, &ServiceId::from(who).service_id_string()))
        .unwrap()
        .roaming
}

/// B1 (critic of the A1 brief), contact: a new message brings an archived chat back (C3b moves
/// the base to 0 in its own zone, written here by hand as it does), and when the phone
/// archives the chat AGAIN its record says archived, exactly as the record before it did.
/// Diffed against the previous record that is no change and the chat would stay in the inbox
/// for ever; diffed against what the core was told it is archived again.
#[test]
fn a_contact_archived_again_on_the_phone_is_archived_after_kourier_brought_it_back() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    core_has(&fixture, &user(ada));
    let archived = |key: u8, muted: u64| {
        vec![contact_keyed(key, ada, move |c| {
            c.archived = true;
            c.muted_until_timestamp = muted;
        })]
    };
    // R1: the phone archives the chat (and mutes it for ever): stated, and the base is what
    // was stated.
    db.zone(|tx| merge_zone(tx, account, &archived(1, u64::MAX), None, true, 10, &wake))
        .unwrap();
    let chat = conversation(&outbox(db), &user(ada)).expect("R1 states the archive");
    assert_eq!(chat.archived, Some(true));
    assert_eq!(
        roaming_of(db, ada),
        Some(peers::Roaming {
            archived: true,
            muted_until: Some(i64::MAX)
        })
    );
    // A new message brings the chat back: the base moves with it, in its zone.
    db.zone(|tx| {
        peers::set_roaming_archived(tx, &ServiceId::from(ada).service_id_string(), false, 20)
    })
    .unwrap();
    assert_eq!(
        roaming_of(db, ada).map(|r| r.archived),
        Some(false),
        "the mute stays what it was told"
    );
    assert_eq!(roaming_of(db, ada).unwrap().muted_until, Some(i64::MAX));
    // R2: the phone archives it again; the record is the same as R1 in every flag.
    db.zone(|tx| merge_zone(tx, account, &archived(2, u64::MAX), None, false, 30, &wake))
        .unwrap();
    let chat = conversation(&outbox(db), &user(ada)).expect("archived again on the phone");
    assert_eq!(chat.archived, Some(true));
    assert_eq!(roaming_of(db, ada).map(|r| r.archived), Some(true));
    // R3: nothing changed since R2 and the core agrees: nothing is said.
    db.zone(|tx| merge_zone(tx, account, &archived(3, u64::MAX), None, false, 40, &wake))
        .unwrap();
    assert!(conversation(&outbox(db), &user(ada)).is_none());
    // R4: the phone unarchives it: stated, and the base follows.
    let open = vec![contact_keyed(4, ada, |_| {})];
    db.zone(|tx| merge_zone(tx, account, &open, None, false, 50, &wake))
        .unwrap();
    let chat = conversation(&outbox(db), &user(ada)).expect("unarchived on the phone");
    assert_eq!(chat.archived, Some(false));
    assert_eq!(
        roaming_of(db, ada),
        Some(peers::Roaming {
            archived: false,
            muted_until: None
        })
    );
}

/// B1, Note to Self: the same loop against the own peer's base (the previous AccountRecord
/// says archived both times).
#[test]
fn note_to_self_archived_again_on_the_phone_is_archived_after_kourier_brought_it_back() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let own = ServiceId::from(account.aci).service_id_string();
    core_has(&fixture, &user(account.aci));
    let archived = |key: u8| {
        vec![item(
            key,
            ItemType::Account,
            Record::Account(proto::AccountRecord {
                note_to_self_archived: true,
                ..Default::default()
            }),
        )]
    };
    db.zone(|tx| merge_zone(tx, account, &archived(1), None, true, 10, &wake))
        .unwrap();
    let note = conversation(&outbox(db), &user(account.aci)).expect("R1 states Note to Self");
    assert_eq!(note.archived, Some(true));
    assert_eq!(
        roaming_of(db, account.aci),
        Some(peers::Roaming {
            archived: true,
            muted_until: None
        })
    );
    db.zone(|tx| peers::set_roaming_archived(tx, &own, false, 20))
        .unwrap();
    db.zone(|tx| merge_zone(tx, account, &archived(2), None, false, 30, &wake))
        .unwrap();
    let note = conversation(&outbox(db), &user(account.aci)).expect("archived again");
    assert_eq!(note.archived, Some(true));
    assert_eq!(roaming_of(db, account.aci).map(|r| r.archived), Some(true));
    // Nothing changed since: nothing is said.
    db.zone(|tx| merge_zone(tx, account, &archived(3), None, false, 40, &wake))
        .unwrap();
    assert!(conversation(&outbox(db), &user(account.aci)).is_none());
}

/// B1, group: `groups.archived` IS the base (the merge writes the flags first and compares
/// with the row it read before); the unarchive of C3b sets it to 0, and the phone's next
/// record, archived again, is stated.
#[test]
fn a_group_archived_again_on_the_phone_is_archived_after_kourier_brought_it_back() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let group = crate::groups::GroupKeys::from_master([7; 32]);
    db.zone(|tx| -> Result<()> {
        crate::groups::ensure(tx, &group, 1)?;
        tx.execute(
            "UPDATE groups SET features_hash = 42 WHERE master_key = ?1",
            [&group.master[..]],
        )?;
        Ok(())
    })
    .unwrap();
    let archived = |key: u8| {
        vec![item(
            key,
            ItemType::Groupv2,
            Record::GroupV2(proto::GroupV2Record {
                master_key: group.master.to_vec(),
                archived: true,
                ..Default::default()
            }),
        )]
    };
    let stated = |events: &[BackendEvent]| -> Vec<Option<bool>> {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(p) if p.key == group.conv_key() => {
                    Some(p.archived)
                }
                _ => None,
            })
            .collect()
    };
    db.zone(|tx| merge_zone(tx, account, &archived(1), None, true, 10, &wake))
        .unwrap();
    assert_eq!(stated(&outbox(db)), [Some(true)]);
    // The unarchive of a new message (C3b writes the base in its zone).
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "UPDATE groups SET archived = 0 WHERE master_key = ?1",
            [&group.master[..]],
        )?;
        Ok(())
    })
    .unwrap();
    db.zone(|tx| merge_zone(tx, account, &archived(2), None, false, 20, &wake))
        .unwrap();
    assert_eq!(
        stated(&outbox(db)),
        [Some(true)],
        "archived again on the phone"
    );
    db.zone(|tx| merge_zone(tx, account, &archived(3), None, false, 30, &wake))
        .unwrap();
    assert!(stated(&outbox(db)).is_empty(), "nothing changed since");
}

/// A1: the phone's "Keep muted chats archived" (AccountRecord field 25) is stored with every
/// account record, and is off until one arrives.
#[test]
fn the_phones_keep_muted_setting_is_stored() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    assert!(
        !db.zone(|tx| keep_muted_chats_archived(tx)).unwrap(),
        "off by default"
    );
    let record = |key: u8, keep: bool| {
        vec![item(
            key,
            ItemType::Account,
            Record::Account(proto::AccountRecord {
                keep_muted_chats_archived: keep,
                ..Default::default()
            }),
        )]
    };
    db.zone(|tx| merge_zone(tx, account, &record(1, true), None, true, 10, &wake))
        .unwrap();
    assert!(db.zone(|tx| keep_muted_chats_archived(tx)).unwrap());
    db.zone(|tx| merge_zone(tx, account, &record(2, false), None, false, 20, &wake))
        .unwrap();
    assert!(
        !db.zone(|tx| keep_muted_chats_archived(tx)).unwrap(),
        "switched off again"
    );
}

/// BQ1 (a): a profile from before migration 0023 has no base. The first message that needs
/// it fills it once from the peer's newest stored record, a chat with no record is not
/// archived (and that is the base, so nothing is decoded again), and Note to Self reads the
/// newest account record.
#[test]
fn a_peer_without_a_base_learns_it_once_from_the_newest_stored_record() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let sid = |aci: Aci| ServiceId::from(aci).service_id_string();
    // Two versions of Ada's record (the newer one muted, archived); Bruno has none.
    let batch = [
        contact_keyed(1, ada, |c| c.given_name = "Ada".to_owned()),
        contact_keyed(2, ada, |c| {
            c.archived = true;
            c.muted_until_timestamp = 5_000;
        }),
        item(
            3,
            ItemType::Account,
            Record::Account(proto::AccountRecord {
                note_to_self_archived: true,
                ..Default::default()
            }),
        ),
    ];
    db.zone(|tx| merge_zone(tx, account, &batch[..2], None, true, 10, &wake))
        .unwrap();
    db.zone(|tx| merge_zone(tx, account, &batch[2..], None, false, 11, &wake))
        .unwrap();
    // As if the profile were older than the column.
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "UPDATE peers SET roaming_archived = NULL, roaming_muted_until = NULL",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(roaming_of(db, ada), None);
    let (filled, recorded) = db
        .zone(|tx| fill_roaming(tx, account, &sid(ada), 20))
        .unwrap();
    assert!(recorded);
    let expected = peers::Roaming {
        archived: true,
        muted_until: Some(5_000),
    };
    assert_eq!(filled, expected);
    assert_eq!(roaming_of(db, ada), Some(expected), "written");
    // No record: not archived, and the base is written so that nothing is decoded again.
    let (none, recorded) = db
        .zone(|tx| fill_roaming(tx, account, &sid(bruno), 21))
        .unwrap();
    assert!(!recorded, "no record gave it");
    assert_eq!(none, peers::Roaming::default());
    assert_eq!(roaming_of(db, bruno), Some(peers::Roaming::default()));
    // Note to Self comes from the account record.
    let (note, recorded) = db
        .zone(|tx| fill_roaming(tx, account, &sid(account.aci), 22))
        .unwrap();
    assert!(recorded);
    assert_eq!(
        note,
        peers::Roaming {
            archived: true,
            muted_until: None
        }
    );
}

/// A rebuilt `core.db` has no archived chat: the base of what the core was told goes, and
/// the records that come again state the flags (maintenance, next to `features_hash`).
#[test]
fn a_rebuilt_core_forgets_what_it_was_told_of_the_archive() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    core_has(&fixture, &user(ada));
    db.zone(|tx| {
        merge_zone(
            tx,
            account,
            &[contact_keyed(1, ada, |c| c.archived = true)],
            None,
            true,
            10,
            &wake,
        )
    })
    .unwrap();
    assert_eq!(roaming_of(db, ada).map(|r| r.archived), Some(true));
    let blobs = std::env::temp_dir();
    crate::maintenance::rebuild_events(db, account, &blobs, 20, &wake).unwrap();
    assert_eq!(
        roaming_of(db, ada),
        None,
        "the new core.db was told nothing"
    );
    assert!(
        db.read(|c| peers::load(c, &ServiceId::from(ada).service_id_string()))
            .unwrap()
            .features_hash
            .is_none()
    );
}

// ---------------------------------------------------------------------------------------------
// A1: the whole loop, on one backend.db and per kind of chat. The phone archives a chat, a new
// message brings it back (`receive/unarchive.rs`), the phone archives it AGAIN: its record
// says archived, as the one before did, and the chat must be archived in the core again.
// ---------------------------------------------------------------------------------------------

/// A contact's chat. The second message after the second archive comes back once more.
#[test]
fn a_contact_chat_goes_round_the_archive_with_the_phone_and_a_new_message() {
    use crate::receive::tests::unarchive::{AT, dispatched, text_row, unarchives};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let chat = ConvKey::Direct(user(ada));
    core_has(&fixture, &user(ada));
    let archived = |key: u8| vec![contact_keyed(key, ada, |c| c.archived = true)];
    db.zone(|tx| merge_zone(tx, account, &archived(1), None, true, 10, &wake))
        .unwrap();
    let stated = conversation(&outbox(db), &user(ada)).expect("R1 states the archive");
    assert_eq!(stated.archived, Some(true));
    // A new message brings the chat back.
    let events = dispatched(&fixture, &text_row(ada, "hello", AT, None));
    assert_eq!(unarchives(&events, &chat), 1, "{events:?}");
    // The phone archives it again: the record is the one before it, flag for flag.
    db.zone(|tx| merge_zone(tx, account, &archived(2), None, false, 20, &wake))
        .unwrap();
    let stated = conversation(&outbox(db), &user(ada)).expect("archived again on the phone");
    assert_eq!(stated.archived, Some(true));
    // And a message brings it back again; one more finds it in the inbox.
    let events = dispatched(&fixture, &text_row(ada, "hello again", AT + 1, None));
    assert_eq!(unarchives(&events, &chat), 1, "{events:?}");
    let events = dispatched(&fixture, &text_row(ada, "and again", AT + 2, None));
    assert_eq!(unarchives(&events, &chat), 0, "{events:?}");
}

/// A group, whose own flag is the base.
#[test]
fn a_group_chat_goes_round_the_archive_with_the_phone_and_a_new_message() {
    use crate::receive::tests::unarchive::{AT, archived_group, dispatched, text_row, unarchives};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let keys = archived_group(&fixture, [7; 32], &[ada], None);
    db.zone(|tx| -> Result<()> {
        tx.execute("UPDATE groups SET archived = 0", [])?;
        Ok(())
    })
    .unwrap();
    let archived = |key: u8| {
        vec![item(
            key,
            ItemType::Groupv2,
            Record::GroupV2(proto::GroupV2Record {
                master_key: keys.master.to_vec(),
                archived: true,
                ..Default::default()
            }),
        )]
    };
    let stated = |events: &[BackendEvent]| -> Vec<Option<bool>> {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(p) if p.key == keys.conv_key() => Some(p.archived),
                _ => None,
            })
            .collect()
    };
    db.zone(|tx| merge_zone(tx, account, &archived(1), None, true, 10, &wake))
        .unwrap();
    assert_eq!(stated(&outbox(db)), [Some(true)]);
    let events = dispatched(&fixture, &text_row(ada, "hello", AT, Some(&keys)));
    assert_eq!(unarchives(&events, &keys.conv_key()), 1, "{events:?}");
    db.zone(|tx| merge_zone(tx, account, &archived(2), None, false, 20, &wake))
        .unwrap();
    assert_eq!(stated(&outbox(db)), [Some(true)], "archived again");
    let events = dispatched(&fixture, &text_row(ada, "hello again", AT + 1, Some(&keys)));
    assert_eq!(unarchives(&events, &keys.conv_key()), 1, "{events:?}");
}

/// Note to Self: our phone's transcript of a note brings it back.
#[test]
fn note_to_self_goes_round_the_archive_with_the_phone_and_a_note() {
    use crate::receive::tests::unarchive::{AT, dispatched, transcript_row, unarchives};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let own = ServiceId::from(account.aci);
    let chat = ConvKey::Direct(user(account.aci));
    core_has(&fixture, &user(account.aci));
    let archived = |key: u8| {
        vec![item(
            key,
            ItemType::Account,
            Record::Account(proto::AccountRecord {
                note_to_self_archived: true,
                ..Default::default()
            }),
        )]
    };
    db.zone(|tx| merge_zone(tx, account, &archived(1), None, true, 10, &wake))
        .unwrap();
    assert_eq!(
        conversation(&outbox(db), &user(account.aci)).map(|c| c.archived),
        Some(Some(true))
    );
    let events = dispatched(&fixture, &transcript_row(account.aci, own, "a note", AT));
    assert_eq!(unarchives(&events, &chat), 1, "{events:?}");
    db.zone(|tx| merge_zone(tx, account, &archived(2), None, false, 20, &wake))
        .unwrap();
    assert_eq!(
        conversation(&outbox(db), &user(account.aci)).map(|c| c.archived),
        Some(Some(true)),
        "archived again on the phone"
    );
}

/// BQ1 (a), the wiring: a profile from before migration 0023 has no base. The first message of
/// a chat the phone archived reads it from the newest stored record, once, and brings the
/// chat back; a chat the phone never archived is left alone and learns "not archived".
#[test]
fn a_profile_from_before_the_base_brings_an_archived_chat_back_at_its_first_message() {
    use crate::receive::tests::unarchive::{AT, base_of, dispatched, text_row, unarchives};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let records = vec![
        contact_keyed(1, ada, |c| {
            c.archived = true;
            c.muted_until_timestamp = 1_234_567_890_123;
        }),
        contact_keyed(2, bruno, |_| {}),
    ];
    db.zone(|tx| merge_zone(tx, account, &records, None, true, 10, &wake))
        .unwrap();
    // The profile is from before the base: nothing was ever told.
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "UPDATE peers SET roaming_archived = NULL, roaming_muted_until = NULL",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(base_of(&fixture, ada), None);
    let events = dispatched(&fixture, &text_row(ada, "hello", AT, None));
    assert_eq!(
        unarchives(&events, &ConvKey::Direct(user(ada))),
        1,
        "{events:?}"
    );
    assert_eq!(
        base_of(&fixture, ada),
        Some(peers::Roaming {
            archived: false,
            muted_until: Some(1_234_567_890_123)
        }),
        "learned from the record, then moved by the unarchive"
    );
    // Bruno has no record at all: nobody archived the chat anywhere, nothing is stated.
    // Bruno's record says "not archived": the base is filled from it and the core is told so
    // once (it may have kept the chat archived across Link Again), a second message is not.
    let events = dispatched(&fixture, &text_row(bruno, "hello", AT + 1, None));
    assert_eq!(
        unarchives(&events, &ConvKey::Direct(user(bruno))),
        1,
        "{events:?}"
    );
    let events = dispatched(&fixture, &text_row(bruno, "hello", AT + 2, None));
    assert_eq!(
        unarchives(&events, &ConvKey::Direct(user(bruno))),
        0,
        "{events:?}"
    );
    assert_eq!(
        base_of(&fixture, bruno),
        Some(peers::Roaming::default()),
        "learned once: not archived"
    );
}

// ---------------------------------------------------------------------------------------------
// A1 (C3c): our own sends, the reset of the Storage Service, the import
// ---------------------------------------------------------------------------------------------

/// How many events of `events` say that `chat` is archived (`true`) or not (`false`).
fn archive_says(events: &[BackendEvent], chat: &ConvKey, archived: bool) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(e, BackendEvent::ConversationUpsert(p)
                if &p.key == chat && p.archived == Some(archived))
        })
        .count()
}

/// Runs the send side of the rule (`send/` calls it once the server accepted a new message).
fn own_send(fixture: &crate::receive::tests::Fixture, chat: &ConvKey, now: i64) {
    let wake = Rc::new(Notify::new());
    fixture
        .db
        .zone(|tx| {
            crate::receive::unarchive::after_own_send(tx, &fixture.account, now, chat, &wake)
        })
        .unwrap();
}

/// BQ5 (a): a new message of ours that went out brings an archived chat back, like an incoming
/// one: ONE upsert in the outbox and the base moved, in the one zone; a send finding the chat
/// in the inbox writes nothing; the phone's mute with "Keep muted chats archived" keeps it
/// archived; and the phone's next record, archived again, archives the chat in the core again.
/// A contact, a group and Note to Self.
#[test]
fn an_own_send_brings_an_archived_chat_back() {
    use crate::receive::tests::unarchive::{archived_group, base_of, group_archived, keep_muted};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let chat = ConvKey::Direct(user(ada));
    core_has(&fixture, &user(ada));
    let archived = |key: u8, muted: u64| {
        vec![contact_keyed(key, ada, move |c| {
            c.archived = true;
            c.muted_until_timestamp = muted;
        })]
    };

    // A contact whose chat the phone archived and muted for ever, "Keep muted chats archived"
    // on: the send leaves it where it is, with nothing said and nothing written.
    db.zone(|tx| merge_zone(tx, account, &archived(1, u64::MAX), None, true, 10, &wake))
        .unwrap();
    outbox(db);
    keep_muted(&fixture, true);
    own_send(&fixture, &chat, 20);
    assert!(outbox(db).is_empty());
    assert_eq!(base_of(&fixture, ada).map(|r| r.archived), Some(true));
    // The setting off: the chat comes back, once, and the base moves (the mute stays).
    keep_muted(&fixture, false);
    own_send(&fixture, &chat, 21);
    let events = outbox(db);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(archive_says(&events, &chat, false), 1, "{events:?}");
    assert_eq!(
        base_of(&fixture, ada),
        Some(peers::Roaming {
            archived: false,
            muted_until: Some(i64::MAX)
        })
    );
    // A second send finds it in the inbox.
    own_send(&fixture, &chat, 22);
    assert!(outbox(db).is_empty());
    // The phone archives it again: its record is R1 flag for flag, and the chat is archived
    // in the core again.
    db.zone(|tx| merge_zone(tx, account, &archived(2, u64::MAX), None, false, 30, &wake))
        .unwrap();
    assert_eq!(
        conversation(&outbox(db), &user(ada)).map(|c| c.archived),
        Some(Some(true))
    );

    // A chat the phone never archived: nothing to bring back, nothing said.
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    core_has(&fixture, &user(bruno));
    own_send(&fixture, &ConvKey::Direct(user(bruno)), 40);
    assert!(outbox(db).is_empty());

    // A group: its own flag is the base.
    let keys = archived_group(&fixture, [7; 32], &[ada], None);
    own_send(&fixture, &keys.conv_key(), 50);
    assert_eq!(archive_says(&outbox(db), &keys.conv_key(), false), 1);
    assert!(!group_archived(&fixture, &keys));
    own_send(&fixture, &keys.conv_key(), 51);
    assert!(outbox(db).is_empty(), "a group in the inbox says nothing");
    let record = vec![item(
        3,
        ItemType::Groupv2,
        Record::GroupV2(proto::GroupV2Record {
            master_key: keys.master.to_vec(),
            archived: true,
            ..Default::default()
        }),
    )];
    db.zone(|tx| merge_zone(tx, account, &record, None, false, 60, &wake))
        .unwrap();
    assert_eq!(archive_says(&outbox(db), &keys.conv_key(), true), 1);

    // Note to Self: the own peer row holds the base.
    let note = ConvKey::Direct(user(account.aci));
    core_has(&fixture, &user(account.aci));
    let note_record = |key: u8| {
        vec![item(
            key,
            ItemType::Account,
            Record::Account(proto::AccountRecord {
                note_to_self_archived: true,
                ..Default::default()
            }),
        )]
    };
    db.zone(|tx| merge_zone(tx, account, &note_record(4), None, false, 70, &wake))
        .unwrap();
    assert_eq!(archive_says(&outbox(db), &note, true), 1);
    own_send(&fixture, &note, 80);
    assert_eq!(archive_says(&outbox(db), &note, false), 1);
    db.zone(|tx| merge_zone(tx, account, &note_record(5), None, false, 90, &wake))
        .unwrap();
    assert_eq!(
        conversation(&outbox(db), &user(account.aci)).map(|c| c.archived),
        Some(Some(true)),
        "archived again on the phone"
    );
}

/// D3 (decisions): `storage::reset` (a new storage key, a rebuilt core.db) forgets the stored
/// records, so the next record of every chat is a FIRST record again. A chat that came back
/// in between is archived again by it (its record says archived; a first record states its
/// flags whatever they are) and the base follows. The groups keep their rows, so a group's
/// diff base survives the reset and its record is compared with it as ever.
#[test]
fn after_a_storage_reset_the_phone_archives_again() {
    use crate::receive::tests::unarchive::{AT, base_of, dispatched, text_row, unarchives};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let chat = ConvKey::Direct(user(ada));
    core_has(&fixture, &user(ada));
    let archived = |key: u8| vec![contact_keyed(key, ada, |c| c.archived = true)];
    db.zone(|tx| merge_zone(tx, account, &archived(1), None, true, 10, &wake))
        .unwrap();
    assert_eq!(archive_says(&outbox(db), &chat, true), 1);
    // A message brings the chat back; the base is 0.
    let events = dispatched(&fixture, &text_row(ada, "hello", AT, None));
    assert_eq!(unarchives(&events, &chat), 1, "{events:?}");
    assert_eq!(base_of(&fixture, ada).map(|r| r.archived), Some(false));
    // The Storage Service starts over; the phone's record is a first record.
    db.zone(|tx| reset(tx, 20)).unwrap();
    let stored = db
        .read(|c| -> Result<i64> {
            Ok(c.query_row("SELECT count(*) FROM storage_records", [], |r| r.get(0))?)
        })
        .unwrap();
    assert_eq!(stored, 0);
    db.zone(|tx| merge_zone(tx, account, &archived(2), None, true, 30, &wake))
        .unwrap();
    assert_eq!(
        archive_says(&outbox(db), &chat, true),
        1,
        "the first record states the archive again"
    );
    assert_eq!(base_of(&fixture, ada).map(|r| r.archived), Some(true));
    // And the message after that brings it back as before.
    let events = dispatched(&fixture, &text_row(ada, "again", AT + 1, None));
    assert_eq!(unarchives(&events, &chat), 1, "{events:?}");
}

/// D10 with the base: the history import states a chat as archived and writes the base it
/// stated (`transfer/deliver.rs`, pinned by `history_import`'s mock test); the FIRST Storage
/// record of that chat then states its flags whatever they are, and the record wins: archived
/// again in both directions, with the base following the record's word and its mute.
#[test]
fn the_import_states_the_archive_and_the_first_record_wins() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    // What the import wrote for two chats it stated as archived.
    for (who, muted) in [(ada, None), (bruno, Some(i64::MAX))] {
        core_has(&fixture, &user(who));
        db.zone(|tx| {
            peers::set_roaming(
                tx,
                &ServiceId::from(who).service_id_string(),
                peers::Roaming {
                    archived: true,
                    muted_until: muted,
                },
                5,
            )
        })
        .unwrap();
    }
    // The phone's first records: Ada's chat is not archived there (the import's word was
    // older), Bruno's is, with a mute of its own.
    let records = vec![
        contact_keyed(1, ada, |_| {}),
        contact_keyed(2, bruno, |c| {
            c.archived = true;
            c.muted_until_timestamp = 4_102_444_800_000;
        }),
    ];
    db.zone(|tx| merge_zone(tx, account, &records, None, true, 10, &wake))
        .unwrap();
    let events = outbox(db);
    assert_eq!(
        archive_says(&events, &ConvKey::Direct(user(ada)), false),
        1,
        "{events:?}"
    );
    assert_eq!(
        archive_says(&events, &ConvKey::Direct(user(bruno)), true),
        1,
        "stated again although the import said so: it is a first record"
    );
    assert_eq!(
        roaming_of(db, ada),
        Some(peers::Roaming {
            archived: false,
            muted_until: None
        })
    );
    assert_eq!(
        roaming_of(db, bruno),
        Some(peers::Roaming {
            archived: true,
            muted_until: Some(4_102_444_800_000)
        })
    );
}

/// S-F1 (the storage review's F1): a profile from before migration 0023 has its newest account
/// record stored (it says "Keep muted chats archived" as the phone has it) and the column
/// empty. The first counting message of a muted, archived chat fills the setting once from that
/// record, so the chat stays archived when it is on, and comes back when it is off: a contact's
/// chat and a group alike.
#[test]
fn a_profile_from_before_the_base_keeps_the_phones_keep_muted_setting() {
    use crate::receive::tests::unarchive::{
        AT, archive_direct, archived_group, dispatched, group_archived, peer, text_row, unarchives,
    };
    for keep in [true, false] {
        let why = format!("the stored account record says keep muted chats archived = {keep}");
        let fixture = fixture();
        let (db, account) = (&fixture.db, &fixture.account);
        let wake = Rc::new(Notify::new());
        let record = vec![item(
            1,
            ItemType::Account,
            Record::Account(proto::AccountRecord {
                keep_muted_chats_archived: keep,
                ..Default::default()
            }),
        )];
        db.zone(|tx| merge_zone(tx, account, &record, None, true, 10, &wake))
            .unwrap();
        // As if the profile were older than the column.
        db.zone(|tx| -> Result<()> {
            tx.execute("UPDATE account SET keep_muted_archived = NULL", [])?;
            Ok(())
        })
        .unwrap();
        let told = || {
            db.read(|c| {
                Ok::<_, crate::error::Error>(c.query_row(
                    "SELECT keep_muted_archived FROM account",
                    [],
                    |r| r.get::<_, Option<bool>>(0),
                )?)
            })
            .unwrap()
        };
        assert_eq!(told(), None);
        // A contact's chat and a group, archived and muted for ever on the phone.
        let ada = peer();
        archive_direct(&fixture, ada, Some(i64::MAX));
        let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
        let keys = archived_group(&fixture, [7; 32], &[bruno], Some(i64::MAX));
        let events = dispatched(&fixture, &text_row(ada, "hello", AT, None));
        assert_eq!(
            unarchives(&events, &ConvKey::Direct(user(ada))) == 1,
            !keep,
            "{why}: {events:?}"
        );
        assert_eq!(told(), Some(keep), "{why}: filled once, from the record");
        let events = dispatched(&fixture, &text_row(bruno, "hello all", AT + 1, Some(&keys)));
        assert_eq!(
            unarchives(&events, &keys.conv_key()) == 1,
            !keep,
            "{why} (group): {events:?}"
        );
        assert_eq!(group_archived(&fixture, &keys), keep, "{why} (group)");
    }
}

/// Re-check N1: a setting filled from NO account record (false) is the phone's word only until
/// an account record arrives; `merge_account` writes what that record says, and a muted
/// archived chat then stays archived.
#[test]
fn a_keep_muted_setting_filled_without_a_record_follows_the_next_account_record() {
    use crate::receive::tests::unarchive::{
        AT, archive_direct, dispatched, peer, text_row, unarchives,
    };
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let told = || {
        db.read(|c| {
            Ok::<_, crate::error::Error>(c.query_row(
                "SELECT keep_muted_archived FROM account",
                [],
                |r| r.get::<_, Option<bool>>(0),
            )?)
        })
        .unwrap()
    };
    assert_eq!(
        told(),
        None,
        "a fresh account row has not heard the setting"
    );
    // No account record yet: the first look fills false, and the muted archived chat comes back.
    let ada = peer();
    archive_direct(&fixture, ada, Some(i64::MAX));
    let events = dispatched(&fixture, &text_row(ada, "hello", AT, None));
    assert_eq!(
        unarchives(&events, &ConvKey::Direct(user(ada))),
        1,
        "{events:?}"
    );
    assert_eq!(told(), Some(false), "filled from no record");
    // The phone's account record says keep muted chats archived: it replaces the filled value.
    let record = vec![item(
        1,
        ItemType::Account,
        Record::Account(proto::AccountRecord {
            keep_muted_chats_archived: true,
            ..Default::default()
        }),
    )];
    db.zone(|tx| merge_zone(tx, account, &record, None, true, 10, &wake))
        .unwrap();
    assert_eq!(told(), Some(true), "the record's word");
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    archive_direct(&fixture, bruno, Some(i64::MAX));
    let events = dispatched(&fixture, &text_row(bruno, "hello", AT + 1, None));
    assert_eq!(
        unarchives(&events, &ConvKey::Direct(user(bruno))),
        0,
        "{events:?}"
    );
}

/// S-F2 (the storage review's F2): after Link Again the backend.db is new and the core still
/// has the chat archived; the phone's first records state nothing (no `features_hash`), so the
/// base of the chat is filled from the record ("not archived") at the first counting message,
/// and that message states `archived: false` once, whatever the filled base says. A second
/// message states nothing. A contact's chat and Note to Self.
#[test]
fn a_filled_base_states_not_archived_once_so_that_a_core_that_kept_the_chat_archived_heals() {
    use crate::receive::tests::unarchive::{AT, dispatched, text_row, transcript_row, unarchives};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    // No `core_has`: the fresh backend.db knows of no chat the core has.
    let records = vec![
        contact_keyed(1, ada, |c| c.given_name = "Ada".to_owned()),
        item(
            2,
            ItemType::Account,
            Record::Account(proto::AccountRecord::default()),
        ),
    ];
    db.zone(|tx| merge_zone(tx, account, &records, None, true, 10, &wake))
        .unwrap();
    let events = outbox(db);
    for flag in [true, false] {
        assert_eq!(
            archive_says(&events, &ConvKey::Direct(user(ada)), flag),
            0,
            "first records state no flag: {events:?}"
        );
    }
    assert_eq!(roaming_of(db, ada), None);
    assert_eq!(roaming_of(db, account.aci), None);

    let chat = ConvKey::Direct(user(ada));
    let events = dispatched(&fixture, &text_row(ada, "hello", AT, None));
    assert_eq!(unarchives(&events, &chat), 1, "{events:?}");
    assert_eq!(roaming_of(db, ada).map(|r| r.archived), Some(false));
    let events = dispatched(&fixture, &text_row(ada, "again", AT + 1, None));
    assert_eq!(unarchives(&events, &chat), 0, "{events:?}");

    // Note to Self: our phone's transcript to our own account.
    let own = ConvKey::Direct(user(account.aci));
    let row = transcript_row(account.aci, account.aci.into(), "note", AT + 2);
    let events = dispatched(&fixture, &row);
    assert_eq!(unarchives(&events, &own), 1, "{events:?}");
    let row = transcript_row(account.aci, account.aci.into(), "note two", AT + 3);
    let events = dispatched(&fixture, &row);
    assert_eq!(unarchives(&events, &own), 0, "{events:?}");
}

/// P1 F2c: a record that names a PNI alone, after the PNI was linked to an ACI whose profile key
/// we hold, states the verdict of that ACI (hidden: a key, no sharing, no address-book name), not
/// the PNI row's "shown": the core resolves the alias, so the wrong word would show the ACI's number.
#[test]
fn a_pni_only_record_states_the_verdict_of_the_aci_it_is_linked_to() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let ada_pni = Pni::from(uuid::Uuid::from_u128(0xada_0001));
    db.zone(|tx| -> Result<()> {
        peers::store_profile_key(tx, ada, &[0x41; 32], 1)?;
        crate::links::learn(tx, account, ada_pni, ada, 1)?;
        Ok(())
    })
    .unwrap();
    let pni_user = crate::service_id::pni_user_id(ada_pni).unwrap();
    let batch = vec![item(
        5,
        ItemType::Contact,
        Record::Contact(proto::ContactRecord {
            pni_binary: uuid::Uuid::from(ada_pni).as_bytes().to_vec(),
            e164: "+15550111".to_owned(),
            ..Default::default()
        }),
    )];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 100, &wake))
        .unwrap();
    let events = outbox(db);
    let patches = upserts(&events, &pni_user);
    assert_eq!(patches.len(), 1, "{events:?}");
    assert_eq!(patches[0].number_shown, Some(false));
}

/// Re-check N4: a PNI-only record that carries an address-book name marks the PNI row; the
/// person is one conversation, so the verdict is the OR of the PNI row's and the linked ACI
/// row's flags (Desktop shows her number): shown although the ACI holds a key and does not share.
#[test]
fn a_pni_only_record_with_an_address_book_name_shows_the_number() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada2));
    let ada_pni = Pni::from(uuid::Uuid::from_u128(0xada2_0001));
    db.zone(|tx| -> Result<()> {
        peers::store_profile_key(tx, ada, &[0x41; 32], 1)?;
        crate::links::learn(tx, account, ada_pni, ada, 1)?;
        Ok(())
    })
    .unwrap();
    let pni_user = crate::service_id::pni_user_id(ada_pni).unwrap();
    let batch = vec![item(
        5,
        ItemType::Contact,
        Record::Contact(proto::ContactRecord {
            pni_binary: uuid::Uuid::from(ada_pni).as_bytes().to_vec(),
            e164: "+15550112".to_owned(),
            system_given_name: "Ada".to_owned(),
            ..Default::default()
        }),
    )];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 100, &wake))
        .unwrap();
    let events = outbox(db);
    let patches = upserts(&events, &pni_user);
    assert_eq!(patches.len(), 1, "{events:?}");
    assert_eq!(
        patches[0].number_shown,
        Some(true),
        "the PNI row is in the address book"
    );
}

/// P1 F5: only an address-book GIVEN or FAMILY name makes a system contact (Desktop's
/// `isInSystemContacts`); a system nickname alone names the person but does not show her number.
#[test]
fn a_system_nickname_alone_is_no_system_contact_for_the_number_rule() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let dana = Aci::from(uuid::Uuid::from_u128(0xda4a));
    let erin = Aci::from(uuid::Uuid::from_u128(0xe121));
    let batch = vec![
        contact_keyed(1, dana, |c| {
            c.e164 = "+15550141".to_owned();
            c.profile_key = vec![0x43; 32];
            c.system_nickname = "Dana from the gym".to_owned();
        }),
        contact_keyed(2, erin, |c| {
            c.e164 = "+15550142".to_owned();
            c.profile_key = vec![0x44; 32];
            c.system_family_name = "Erin".to_owned();
        }),
    ];
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 100, &wake))
        .unwrap();
    let events = outbox(db);
    let dana_patch = &upserts(&events, &user(dana))[0];
    assert_eq!(
        dana_patch.system_name,
        Patch::Set("Dana from the gym".to_owned()),
        "the nickname still names her"
    );
    assert_eq!(dana_patch.number_shown, Some(false));
    assert_eq!(
        upserts(&events, &user(erin))[0].number_shown,
        Some(true),
        "a family name is the address book"
    );
}

/// P5 critic H1 / M2.2-M2.4: a record for the STORED key that moves it out of state 2 releases
/// (`IdentityTrusted`, the waiting jobs due, the runner woken) and leaves one quiet notice "from
/// another device" (id `verified:<hex8(sha256(key))>:<state>:<merge time>`); while this device's own
/// Verified sync for the same key is still queued, an older record changes nothing; a record
/// with another key stores nothing and queues a profile fetch.
#[test]
fn an_unverified_record_set_default_releases_with_a_notice_unless_our_click_waits() {
    use kourier_backend_api::BackendNotice;
    use kourier_model::{Body, IdentityFingerprint, Origin, SystemEvent};
    use proto::contact_record::IdentityState;
    use sha2::{Digest as _, Sha256};
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let sid = ServiceId::from(ada).service_id_string();
    let key = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng())
        .identity_key()
        .serialize()
        .to_vec();
    let state = || {
        db.read(|c| -> Result<(Vec<u8>, i64)> {
            Ok(c.query_row(
                "SELECT identity_key, verified FROM identities WHERE service_id = ?1",
                [&sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap()
    };
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
             VALUES (?1, ?2, 2, 0, 1)",
            (&sid, &key[..]),
        )?;
        crate::peers::set_features_hash(tx, &sid, 7, 1)?;
        tx.execute(
            "INSERT INTO jobs(kind, key, priority, next_at, created_at, last_error)
             VALUES ('profile_fetch', 'waiting', 1, 9999999, 0, ?1)",
            [crate::jobs::UNTRUSTED_ERROR],
        )?;
        Ok(())
    })
    .unwrap();
    let record = |raw: u8, key: &[u8], identity: IdentityState| {
        contact_keyed(raw, ada, |c| {
            c.given_name = "A".to_owned();
            c.whitelisted = true;
            c.identity_key = key.to_vec();
            c.identity_state = identity as i32;
        })
    };
    let notices = |events: &[BackendEvent]| -> Vec<kourier_backend_api::IncomingMessage> {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m)
                    if matches!(m.body, Body::System(SystemEvent::VerifiedChanged { .. })) =>
                {
                    Some(m.clone())
                }
                _ => None,
            })
            .collect()
    };
    let trusted = |events: &[BackendEvent]| {
        events.iter().any(|e| {
            matches!(e, BackendEvent::Notice(BackendNotice::IdentityTrusted { user: u }) if *u == user(ada))
        })
    };

    // 1. The phone set the changed key to DEFAULT.
    let outcome = db
        .zone(|tx| {
            merge_zone(
                tx,
                account,
                &[record(0x51, &key, IdentityState::Default)],
                None,
                false,
                100,
                &wake,
            )
        })
        .unwrap();
    let events = outbox(db);
    assert!(
        state() == (key.clone(), 0),
        "the stored key must stay, its state DEFAULT"
    );
    assert!(
        trusted(&events),
        "leaving state 2 must tell the core to release"
    );
    assert!(
        outcome.released,
        "the runner must be woken for the released jobs"
    );
    let due: i64 = db
        .read(|c| -> Result<i64> {
            Ok(
                c.query_row("SELECT next_at FROM jobs WHERE key = 'waiting'", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap();
    assert!(due <= 100, "the waiting job must be due again");
    let notes = notices(&events);
    assert!(notes.len() == 1, "one notice, got {}", notes.len());
    let digest: String = Sha256::digest(&key)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(
        &*notes[0].local == format!("verified:{digest}:0:100").as_str(),
        "the Storage notice id must be verified:<hex8(sha256(key))>:<state>:<merge time>"
    );
    assert!(notes[0].silent, "the notice must be silent");
    assert!(
        notes[0].origin
            == Origin::Remote {
                author: user(ada),
                device: None
            },
        "the notice must be authored by the contact"
    );
    assert!(
        notes[0].body
            == Body::System(SystemEvent::VerifiedChanged {
                user: user(ada),
                verified: false,
                other_device: true,
            }),
        "the notice must say 'not verified, from another device'"
    );

    // 2. Mark as Verified here; the phone's record still says DEFAULT (it has not taken our
    // sync yet): the record changes nothing.
    db.zone(|tx| {
        crate::safety::set_verified(
            tx,
            account,
            &user(ada),
            true,
            &IdentityFingerprint(key.clone()),
            200,
            1,
            &wake,
        )
    })
    .unwrap();
    let _ = outbox(db);
    db.zone(|tx| {
        merge_zone(
            tx,
            account,
            &[record(0x52, &key, IdentityState::Default)],
            None,
            false,
            300,
            &wake,
        )
    })
    .unwrap();
    let events = outbox(db);
    assert!(
        state() == (key.clone(), 1),
        "our own queued mark must win over an older record"
    );
    assert!(notices(&events).is_empty(), "no notice for an older record");

    // 3. A record with another key: nothing stored, a profile fetch.
    let other = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng())
        .identity_key()
        .serialize()
        .to_vec();
    let outcome = db
        .zone(|tx| {
            merge_zone(
                tx,
                account,
                &[record(0x53, &other, IdentityState::Verified)],
                None,
                false,
                400,
                &wake,
            )
        })
        .unwrap();
    let events = outbox(db);
    assert!(
        state() == (key.clone(), 1),
        "another key must never replace the stored one"
    );
    assert!(
        outcome.profile_fetches,
        "another key queues a profile fetch"
    );
    assert!(notices(&events).is_empty() && !trusted(&events));
}

/// The lead's R10 (storage review LOW-2): a phone that marks Ada Verified, clears, and marks
/// Verified again, each seen through a Storage record, leaves THREE notices: their ids differ
/// (the merge time is in the id), so the core's idempotent ingest keeps all three.
#[test]
fn a_phone_toggling_back_leaves_a_notice_each_time() {
    use kourier_model::{Body, SystemEvent};
    use proto::contact_record::IdentityState;
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let sid = ServiceId::from(ada).service_id_string();
    let key = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng())
        .identity_key()
        .serialize()
        .to_vec();
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "INSERT INTO identities(service_id, identity_key, verified, first_use, updated_at)
             VALUES (?1, ?2, 0, 0, 1)",
            (&sid, &key[..]),
        )?;
        crate::peers::set_features_hash(tx, &sid, 7, 1)?;
        Ok(())
    })
    .unwrap();
    let mut ids = Vec::new();
    for (raw, identity, verified, now) in [
        (0x71, IdentityState::Verified, true, 100),
        (0x72, IdentityState::Default, false, 200),
        (0x73, IdentityState::Verified, true, 300),
    ] {
        let record = contact_keyed(raw, ada, |c| {
            c.given_name = "A".to_owned();
            c.whitelisted = true;
            c.identity_key = key.clone();
            c.identity_state = identity as i32;
        });
        db.zone(|tx| merge_zone(tx, account, &[record], None, false, now, &wake))
            .unwrap();
        let notes: Vec<_> = outbox(db)
            .into_iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) => match m.body {
                    Body::System(SystemEvent::VerifiedChanged { verified: v, .. }) => {
                        assert!(v == verified, "the notice says what the record set");
                        Some(m.local)
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(notes.len() == 1, "one notice per applied change");
        ids.push(notes[0].to_string());
    }
    assert!(
        ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
        "the three notices must have three different ids"
    );
}

// ---------------------------------------------------------------------------------------------
// Targets (A10, migration 0025)
// ---------------------------------------------------------------------------------------------

/// The target column of every stored record, by the first byte of its key.
fn stored_targets(db: &kourier_db::ZoneDb) -> BTreeMap<u8, Option<String>> {
    db.read(|c| -> Result<_> {
        let mut stmt = c.prepare("SELECT key, target FROM storage_records")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<Vec<(Vec<u8>, Option<String>)>>>()?;
        Ok(rows.into_iter().map(|(key, t)| (key[0], t)).collect())
    })
    .unwrap()
}

/// One record of every shape a target is decided for, keyed 1 to 10 (synthetic ids only).
fn records_of_every_target_shape(account: &Account) -> Vec<Decrypted> {
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let ada_pni = Pni::from(uuid::Uuid::from_u128(0xada_0001));
    let dora_pni = Pni::from(uuid::Uuid::from_u128(0xd0_0a));
    let pni_bytes = |pni: Pni| uuid::Uuid::from(pni).as_bytes().to_vec();
    vec![
        // 1: an ACI with its PNI: the ACI is the target.
        contact_keyed(1, ada, |c| c.pni_binary = pni_bytes(ada_pni)),
        // 2: a PNI alone.
        item(
            2,
            ItemType::Contact,
            Record::Contact(proto::ContactRecord {
                pni_binary: pni_bytes(dora_pni),
                ..Default::default()
            }),
        ),
        // 3-5: ourselves (ACI, PNI) and nobody: not a target.
        contact_keyed(3, account.aci, |_| {}),
        item(
            4,
            ItemType::Contact,
            Record::Contact(proto::ContactRecord {
                pni_binary: pni_bytes(account.pni),
                ..Default::default()
            }),
        ),
        item(
            5,
            ItemType::Contact,
            Record::Contact(proto::ContactRecord::default()),
        ),
        // 6-7: a group with a master key, and one whose key has the wrong length.
        item(
            6,
            ItemType::Groupv2,
            Record::GroupV2(proto::GroupV2Record {
                master_key: vec![0xab; 32],
                ..Default::default()
            }),
        ),
        item(
            7,
            ItemType::Groupv2,
            Record::GroupV2(proto::GroupV2Record {
                master_key: vec![0x09; 5],
                ..Default::default()
            }),
        ),
        // 8: the account record.
        item(
            8,
            ItemType::Account,
            Record::Account(proto::AccountRecord::default()),
        ),
        // 9-10: kinds the engine never edits.
        item(
            9,
            ItemType::StoryDistributionList,
            Record::StoryDistributionList(Default::default()),
        ),
        item(10, ItemType::CallLink, Record::CallLink(Default::default())),
    ]
}

/// The target every record of [`records_of_every_target_shape`] must get, spelled out (not built
/// with the functions under test).
fn expected_targets() -> BTreeMap<u8, Option<String>> {
    let some = |s: &str| Some(s.to_owned());
    BTreeMap::from([
        (1, some("contact:00000000-0000-0000-0000-000000000ada")),
        (2, some("contact:PNI:00000000-0000-0000-0000-00000000d00a")),
        (3, some("")),
        (4, some("")),
        (5, some("")),
        (6, some(&format!("group:{}", "ab".repeat(32)))),
        (7, some("")),
        (8, some("account")),
        (9, some("")),
        (10, some("")),
    ])
}

/// A10 (backend.db 25): every record a merge zone stores says what it belongs to, decided from
/// the record alone: `contact:<service id>` (the ACI, else the PNI), `group:<master key hex>`,
/// `account`, and the empty string for a record nobody can ask to change (ourselves, no id, a
/// group key of the wrong length, a kind the engine never edits). The `peer` column and a contact
/// target never disagree.
#[test]
fn a_merged_record_knows_its_target() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let batch = records_of_every_target_shape(account);
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 100, &wake))
        .unwrap();
    assert_eq!(stored_targets(db), expected_targets());
    let rows: Vec<(Option<String>, Option<String>)> = db
        .read(|c| -> Result<_> {
            let mut stmt = c.prepare("SELECT peer, target FROM storage_records")?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(rows.len(), 10);
    for (peer, target) in rows {
        let target = target.expect("every merged row has a target");
        match peer {
            Some(peer) => assert_eq!(target, format!("contact:{peer}")),
            None => assert!(
                !target.starts_with("contact:"),
                "a contact target without a peer"
            ),
        }
    }
}

/// A record delivered again under a key the table already holds (a sync that died after a zone,
/// a row from before migration 0025) takes the target of the record it now holds.
#[test]
fn a_row_delivered_again_takes_the_target_of_its_record() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    for (key, old_target) in [(5u8, None), (6, Some("contact:stale"))] {
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO storage_records(key, kind, record, target, updated_at)
                 VALUES (?1, 1, x'', ?2, 1)",
                (vec![key; 16], old_target),
            )?;
            Ok(())
        })
        .unwrap();
        let again = vec![contact_keyed(key, ada, |_| {})];
        db.zone(|tx| merge_zone(tx, account, &again, None, false, 50, &wake))
            .unwrap();
        assert_eq!(
            stored_targets(db)[&key].as_deref(),
            Some("contact:00000000-0000-0000-0000-000000000ada"),
            "key {key}"
        );
    }
}

/// `fill_targets` on rows from before migration 0025 (every target NULL) gives each the target
/// the merge would have written, decodes each row once, never touches a row that has a target,
/// and leaves a row it cannot decode for another day.
#[test]
fn fill_targets_gives_an_old_row_the_target_the_merge_would_have_written() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let batch = records_of_every_target_shape(account);
    db.zone(|tx| merge_zone(tx, account, &batch, None, true, 100, &wake))
        .unwrap();
    let merged = stored_targets(db);
    assert_eq!(merged, expected_targets());

    // The table as a profile of schema 24 left it, plus two rows of other shapes.
    let kept = contact_keyed(0x21, ada, |_| {});
    db.zone(|tx| -> Result<()> {
        tx.execute("UPDATE storage_records SET target = NULL", [])?;
        tx.execute(
            "INSERT INTO storage_records(key, kind, record, target, updated_at)
             VALUES (?1, 1, x'ff', NULL, 1)",
            [vec![0x20u8; 16]],
        )?;
        tx.execute(
            "INSERT INTO storage_records(key, kind, record, target, updated_at)
             VALUES (?1, 1, ?2, 'contact:kept', 1)",
            (vec![0x21u8; 16], kept.raw.as_slice()),
        )?;
        Ok(())
    })
    .unwrap();
    let filled = db.zone(|tx| fill_targets(tx, account)).unwrap();
    assert_eq!(filled, 10, "the ten decodable rows without a target");
    let mut after = stored_targets(db);
    assert_eq!(after.remove(&0x20), Some(None), "undecodable: not guessed");
    assert_eq!(
        after.remove(&0x21),
        Some(Some("contact:kept".to_owned())),
        "a row with a target is not read again"
    );
    assert_eq!(after, merged, "the same targets the merge wrote");

    // Once: a filled row is not decoded again (its bytes are broken here, its target stays),
    // and the undecodable row is the only one asked about again.
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "UPDATE storage_records SET record = x'ff' WHERE key = ?1",
            [vec![1u8; 16]],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(db.zone(|tx| fill_targets(tx, account)).unwrap(), 0);
    assert_eq!(stored_targets(db)[&1], merged[&1]);
}
