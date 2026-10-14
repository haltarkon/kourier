//! Contact-sync blob parsing and the contact zones.

use std::collections::HashMap;
use std::rc::Rc;

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_model::Patch;
use libsignal_core::Aci;
use tokio::sync::Notify;

use super::*;
use crate::attachment::tests::encrypt;
use crate::attachment::{AttachmentKeys, Integrity, decrypt_in_memory};
use crate::receive::tests::fixture;

/// Desktop's framing (`ContactsParser`, the mock's `serializeContacts`): a varint length, the
/// `ContactDetails`, then `avatar.length` raw bytes.
fn blob(contacts: &[(proto::ContactDetails, Option<&[u8]>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (details, avatar) in contacts {
        let mut details = details.clone();
        if let Some(avatar) = avatar {
            details.avatar = Some(proto::contact_details::Avatar {
                content_type: Some("image/png".to_owned()),
                length: Some(u32::try_from(avatar.len()).unwrap()),
            });
        }
        details.encode_length_delimited(&mut out).unwrap();
        if let Some(avatar) = avatar {
            out.extend_from_slice(avatar);
        }
    }
    out
}

fn details(aci: Aci, name: Option<&str>, number: Option<&str>) -> proto::ContactDetails {
    proto::ContactDetails {
        aci_binary: Some(uuid::Uuid::from(aci).as_bytes().to_vec()),
        name: name.map(str::to_owned),
        number: number.map(str::to_owned),
        ..Default::default()
    }
}

#[test]
fn blobs_parse_with_and_without_avatars_after_decryption() {
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let avatar: Vec<u8> = (0..300u16).map(|i| (i % 256) as u8).collect();
    let plain = blob(&[
        (
            details(ada, Some("Ada Lindqvist"), Some("+15550111")),
            Some(&avatar),
        ),
        (details(bruno, None, None), None),
    ]);
    let key = [8u8; 64];
    let (sealed, digest) = encrypt(&key, [6; 16], &plain, true);
    let decrypted = decrypt_in_memory(
        &AttachmentKeys::from_slice(&key).unwrap(),
        Integrity::Digest(digest),
        plain.len() as u64,
        &sealed,
        MAX_BLOB_LEN,
    )
    .unwrap();
    let parsed = parse(&decrypted).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].details.name.as_deref(), Some("Ada Lindqvist"));
    assert_eq!(
        parsed[0].avatar.as_deref().map(Vec::as_slice),
        Some(&avatar[..])
    );
    assert!(parsed[1].avatar.is_none());
    assert!(parse(&[]).unwrap().is_empty());
    // Truncations are refused, never misread.
    assert_eq!(
        parse(&plain[..plain.len() - 1]).unwrap_err(),
        ParseError::Length
    );
    let first_len = plain.len() - blob(&[(details(bruno, None, None), None)]).len();
    assert_eq!(
        parse(&plain[..first_len - 10]).unwrap_err(),
        ParseError::Avatar
    );
    assert_eq!(parse(&[0x80, 0x80]).unwrap_err(), ParseError::Length);
    assert_eq!(parse(&[0xff; 6]).unwrap_err(), ParseError::Length);
}

fn events(db: &kourier_db::ZoneDb) -> Vec<BackendEvent> {
    let events = db
        .read(|c| kourier_db::outbox::peek(c, 1000))
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

#[test]
fn zones_name_contacts_mark_system_contacts_and_full_syncs_clear_the_rest() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let dir = tempfile::tempdir().unwrap();
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let first = [
        Parsed {
            details: details(ada, Some(" Ada Lindqvist "), Some("+15550111")),
            avatar: None,
        },
        Parsed {
            // The chat with Bruno has a one-week timer on the phone (its third change).
            details: proto::ContactDetails {
                expire_timer: Some(604_800),
                expire_timer_version: Some(3),
                ..details(bruno, Some("Bruno"), Some("not a number"))
            },
            avatar: None,
        },
        Parsed {
            details: details(account.aci, Some("Me"), None),
            avatar: None,
        },
    ];
    let contacts: Vec<_> = first.iter().filter_map(|p| checked(account, p)).collect();
    assert_eq!(contacts.len(), 2, "our own ACI is not a contact");
    let image = b"png bytes".to_vec();
    let written = avatar::write(dir.path(), &image).unwrap();
    let hash: [u8; 32] = Sha256::digest(&image).into();
    let files = HashMap::from([(ServiceId::from(ada).service_id_string(), (written, hash))]);
    db.zone(|tx| {
        apply_zone(
            tx,
            account,
            dir.path(),
            &contacts,
            &files,
            Some(true),
            10,
            &wake,
        )
    })
    .unwrap();
    let sent = events(db);
    let patches: Vec<_> = sent
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ContactUpsert(p) => Some(p.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(patches.len(), 2);
    assert_eq!(
        patches[0].system_name,
        Patch::Set("Ada Lindqvist".to_owned())
    );
    assert_eq!(
        patches[0].identifiers.as_ref().map(|i| i[0].value.as_str()),
        Some("+15550111")
    );
    assert!(matches!(patches[0].avatar, Patch::Set(_)));
    assert_eq!(
        patches[1].identifiers, None,
        "malformed numbers are not identifiers"
    );
    assert!(
        !sent
            .iter()
            .any(|e| matches!(e, BackendEvent::ConversationUpsert(_))),
        "contacts alone create no chats"
    );
    // The initial sync's timers are kept for what the backend sends (and for the chat's first
    // features): Bruno's with its version, Ada's off at Desktop's first version.
    let timers = db
        .read(|c| -> Result<_> {
            let of = |aci: Aci| peers::load(c, &ServiceId::from(aci).service_id_string());
            Ok((of(bruno)?, of(ada)?))
        })
        .unwrap();
    assert_eq!(
        (timers.0.expire_timer, timers.0.expire_timer_version),
        (604_800, 3)
    );
    assert_eq!(
        (timers.1.expire_timer, timers.1.expire_timer_version),
        (0, 1)
    );
    db.read(|c| -> Result<()> {
        let done: bool = c.query_row("SELECT contact_sync_done FROM account", [], |r| r.get(0))?;
        assert!(done);
        for aci in [ada, bruno] {
            let peer = peers::load(c, &ServiceId::from(aci).service_id_string())?;
            assert!(
                peer.system_contact && peer.accepted(),
                "named contacts are accepted"
            );
        }
        let (hash, source) = avatar_state(c, &ServiceId::from(ada).service_id_string())?;
        assert_eq!(
            (hash.map(|h| h.len()), source),
            (Some(32), Some(Source::ContactSync))
        );
        Ok(())
    })
    .unwrap();

    // Bruno has a chat in the core (accepted through the address book).
    let bruno_user = aci_user_id(bruno).unwrap();
    db.zone(|tx| features::features_event(tx, account, &bruno_user, 15))
        .unwrap()
        .expect("the chat's features");

    // A second full sync without Bruno: his address-book name goes, Ada's stays.
    let second = [Parsed {
        details: details(ada, Some("Ada Lindqvist"), None),
        avatar: None,
    }];
    let contacts: Vec<_> = second.iter().filter_map(|p| checked(account, p)).collect();
    db.zone(|tx| {
        apply_zone(
            tx,
            account,
            dir.path(),
            &contacts,
            &HashMap::new(),
            Some(true),
            20,
            &wake,
        )
    })
    .unwrap();
    let sent = events(db);
    assert!(sent.iter().any(|e| matches!(e,
        BackendEvent::ContactUpsert(p) if p.user == bruno_user && p.system_name == Patch::Clear)));
    // His chat is no longer accepted, and the core learns it.
    assert!(
        sent.iter().any(|e| matches!(e,
            BackendEvent::ConversationUpsert(p)
                if p.key == ConvKey::Direct(bruno_user.clone())
                    && p.features.as_ref().and_then(|f| f.cannot_send.clone())
                        == Some(kourier_model::CannotSend::NotAccepted))),
        "{sent:?}"
    );
    db.read(|c| -> Result<()> {
        assert!(!peers::load(c, &ServiceId::from(bruno).service_id_string())?.accepted());
        assert!(peers::load(c, &ServiceId::from(ada).service_id_string())?.accepted());
        Ok(())
    })
    .unwrap();
}

/// Finding 3 of the reviews (hazard 8): the initial contact sync never LOWERS a stored timer
/// version, as Android applies a synced timer only when its version is higher. A history
/// import gave the chat the phone's newer timer; the contact sync that was read earlier on
/// the phone must leave that pair alone and tell the core nothing. A contact with a higher
/// version (or the same) is taken, and the core hears of a changed timer.
#[test]
fn the_initial_contact_sync_never_lowers_a_stored_timer_version() {
    let fixture = fixture();
    let (db, account) = (&fixture.db, &fixture.account);
    let dir = tempfile::tempdir().unwrap();
    let wake = Rc::new(Notify::new());
    let ada = Aci::from(uuid::Uuid::from_u128(0xada));
    let bruno = Aci::from(uuid::Uuid::from_u128(0xb0b));
    let sid = |aci: Aci| ServiceId::from(aci).service_id_string();
    // Both chats are in the core, with the timers an import gave them: Ada's hour at the
    // version 5, Bruno's day at the version 2.
    db.zone(|tx| -> Result<()> {
        for (aci, timer, version) in [(ada, 3_600, 5), (bruno, 86_400, 2)] {
            peers::ensure(tx, &sid(aci), 1)?;
            peers::set_expire_timer(tx, &sid(aci), timer, version, 1)?;
            peers::set_features_hash(tx, &sid(aci), 7, 1)?;
        }
        Ok(())
    })
    .unwrap();
    let stated = |aci: Aci, secs: u32, version: u32| Parsed {
        details: proto::ContactDetails {
            expire_timer: Some(secs),
            expire_timer_version: Some(version),
            ..details(aci, Some("A person"), None)
        },
        avatar: None,
    };
    // Ada: a minute at the version 3 is older than what the core has. Bruno: an hour at the
    // version 4 is newer. (Not the last zone: the sync stays the initial one.)
    let parsed = [stated(ada, 60, 3), stated(bruno, 3_600, 4)];
    let contacts: Vec<_> = parsed.iter().filter_map(|p| checked(account, p)).collect();
    db.zone(|tx| {
        apply_zone(
            tx,
            account,
            dir.path(),
            &contacts,
            &HashMap::new(),
            None,
            10,
            &wake,
        )
    })
    .unwrap();
    let pair = |aci: Aci| {
        let row = db.read(|c| peers::load(c, &sid(aci))).unwrap();
        (row.expire_timer, row.expire_timer_version)
    };
    assert_eq!(
        pair(ada),
        (3_600, 5),
        "the older pair leaves the stored one"
    );
    assert_eq!(pair(bruno), (3_600, 4), "the newer pair is taken");
    let sent = events(db);
    let timer_of = |aci: Aci| {
        sent.iter().find_map(|event| match event {
            BackendEvent::ConversationUpsert(patch)
                if patch.key == ConvKey::Direct(aci_user_id(aci).unwrap())
                    && patch.timer != Patch::Unchanged =>
            {
                Some((patch.timer, patch.timer_version))
            }
            _ => None,
        })
    };
    assert_eq!(timer_of(ada), None, "the core is told nothing of Ada's");
    assert_eq!(
        timer_of(bruno),
        Some((Patch::Set(Duration::from_secs(3_600)), Some(4)))
    );
}

/// Which picture a contact shows follows the phone's setting: profile pictures first, or the
/// address-book photos when `preferContactAvatars` is on.
#[test]
fn an_address_book_photo_replaces_a_profile_picture_only_when_the_setting_prefers_it() {
    // No avatar yet, or an older address-book photo: the new photo is taken either way.
    for prefer in [false, true] {
        assert!(takes_contact_photo(None, false, prefer));
        assert!(takes_contact_photo(
            Some(Source::ContactSync),
            false,
            prefer
        ));
        // The photo already shown is not written again.
        assert!(!takes_contact_photo(
            Some(Source::ContactSync),
            true,
            prefer
        ));
    }
    // Over a profile picture only with the setting.
    assert!(!takes_contact_photo(Some(Source::Profile), false, false));
    assert!(takes_contact_photo(Some(Source::Profile), false, true));
}
