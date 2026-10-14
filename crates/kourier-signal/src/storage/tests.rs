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
