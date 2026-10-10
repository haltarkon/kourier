//! Store conformance (§6.14 items 14-15): identity changes archive sibling sessions, Kyber
//! last-resort triples reject reuse, one-time keys go away, zones roll back store writes.

use std::time::SystemTime;

use kourier_db::ZoneDb;
use libsignal_protocol::{
    CiphertextMessageType, DeviceId, GenericSignedPreKey as _, IdentityChange, IdentityKeyPair,
    IdentityKeyStore as _, KeyPair, KyberPreKeyStore as _, PreKeyBundle, PreKeyRecord,
    PreKeyStore as _, ProtocolAddress, SessionStore as _, SessionUsabilityRequirements,
    SignalProtocolError, Timestamp, process_prekey_bundle,
};

use super::*;
use crate::error::Error;
use crate::keys::{generate_kyber_prekey, generate_signed_prekey};

const THEM: &str = "9d0652a3-dcc3-4d11-975f-74d61598733f";

fn db() -> (tempfile::TempDir, ZoneDb) {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::open(
        &dir.path().join("backend.db"),
        &kourier_backend_api::DbKey::new([1; 32]),
    )
    .unwrap();
    (dir, db)
}

fn ours() -> (IdentityKeyPair, ProtocolAddress) {
    let identity = IdentityKeyPair::generate(&mut rand::rng());
    let local = ProtocolAddress::new(
        "11111111-2222-4333-8444-555555555555".to_owned(),
        DeviceId::new(2).unwrap(),
    );
    (identity, local)
}

/// A prekey bundle of `their` identity for `device`.
fn bundle(their: &IdentityKeyPair, device: u8) -> PreKeyBundle {
    let rng = &mut rand::rng();
    let signed = generate_signed_prekey(their, 1, Timestamp::from_epoch_millis(1), rng).unwrap();
    let kyber = generate_kyber_prekey(their, 2).unwrap();
    PreKeyBundle::new(
        7,
        DeviceId::new(device).unwrap(),
        None,
        signed.id().unwrap(),
        signed.public_key().unwrap(),
        signed.signature().unwrap(),
        kyber.id().unwrap(),
        kyber.public_key().unwrap(),
        kyber.signature().unwrap(),
        *their.identity_key(),
    )
    .unwrap()
}

fn has_current_session(db: &ZoneDb, device: u8) -> bool {
    db.read(|c| -> crate::error::Result<bool> {
        let stores = Stores::new(c, IdentityKind::Aci, ours().0, 1, 0);
        let address = ProtocolAddress::new(THEM.to_owned(), DeviceId::new(device).unwrap());
        Ok(
            ready(stores.session.load_session(&address))??.is_some_and(|record| {
                record
                    .has_usable_sender_chain(
                        SystemTime::now(),
                        SessionUsabilityRequirements::empty(),
                    )
                    .unwrap()
            }),
        )
    })
    .unwrap()
}

#[test]
fn identity_change_archives_sibling_sessions() {
    let (_dir, db) = db();
    let (identity, local) = ours();
    let them = IdentityKeyPair::generate(&mut rand::rng());
    db.zone(|tx| -> crate::error::Result<()> {
        let mut stores = Stores::new(tx, IdentityKind::Aci, identity, 1, 0);
        for device in [1, 2, 3] {
            let remote = ProtocolAddress::new(THEM.to_owned(), DeviceId::new(device).unwrap());
            ready(process_prekey_bundle(
                &remote,
                &local,
                &mut stores.session,
                &mut stores.identity,
                &bundle(&them, device),
                SystemTime::now(),
                &mut rand::rng(),
            ))??;
        }
        assert!(
            stores.identity.take_changes().is_empty(),
            "first use is no change"
        );
        Ok(())
    })
    .unwrap();
    assert!((1..=3).all(|d| has_current_session(&db, d)));

    // Device 1 shows up with a new identity key (reinstall): devices 2 and 3 were built on the
    // old key and are archived; device 1's own session is replaced by libsignal itself.
    let new_identity = IdentityKeyPair::generate(&mut rand::rng());
    db.zone(|tx| -> crate::error::Result<()> {
        let mut stores = Stores::new(tx, IdentityKind::Aci, identity, 1, 0);
        let remote = ProtocolAddress::new(THEM.to_owned(), DeviceId::new(1).unwrap());
        let change = ready(
            stores
                .identity
                .save_identity(&remote, new_identity.identity_key()),
        )??;
        assert_eq!(change, IdentityChange::ReplacedExisting);
        let changes = stores.identity.take_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].service_id, THEM);
        assert_eq!(&changes[0].identity, new_identity.identity_key());
        // Saving the same key again is no change.
        let again = ready(
            stores
                .identity
                .save_identity(&remote, new_identity.identity_key()),
        )??;
        assert_eq!(again, IdentityChange::NewOrUnchanged);
        Ok(())
    })
    .unwrap();
    assert!(has_current_session(&db, 1));
    assert!(!has_current_session(&db, 2));
    assert!(!has_current_session(&db, 3));

    // Sending to the old key is no longer trusted; receiving always is.
    db.read(|c| -> crate::error::Result<()> {
        let stores = Stores::new(c, IdentityKind::Aci, identity, 1, 0);
        let remote = ProtocolAddress::new(THEM.to_owned(), DeviceId::new(2).unwrap());
        use libsignal_protocol::Direction::{Receiving, Sending};
        let trusted = |key: &IdentityKeyPair, direction| {
            ready(
                stores
                    .identity
                    .is_trusted_identity(&remote, key.identity_key(), direction),
            )
        };
        assert!(!trusted(&them, Sending)??);
        assert!(trusted(&new_identity, Sending)??);
        assert!(trusted(&them, Receiving)??);
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_verified_contact_with_a_new_key_needs_confirmation_before_sending() {
    use libsignal_protocol::Direction::{Receiving, Sending};
    let (_dir, db) = db();
    let (identity, _) = ours();
    let old = IdentityKeyPair::generate(&mut rand::rng());
    let new = IdentityKeyPair::generate(&mut rand::rng());
    let remote = ProtocolAddress::new(THEM.to_owned(), DeviceId::new(1).unwrap());
    db.zone(|tx| -> crate::error::Result<()> {
        let mut stores = Stores::new(tx, IdentityKind::Aci, identity, 1, 0);
        ready(stores.identity.save_identity(&remote, old.identity_key()))??;
        // The user compared safety numbers (verified sync or the safety-number screen).
        tx.execute(
            "UPDATE identities SET verified = 1 WHERE service_id = ?1",
            [THEM],
        )?;
        let mut stores = Stores::new(tx, IdentityKind::Aci, identity, 1, 0);
        let trusted = |stores: &Stores<'_>, key: &IdentityKeyPair, direction| {
            ready(
                stores
                    .identity
                    .is_trusted_identity(&remote, key.identity_key(), direction),
            )
        };
        assert!(trusted(&stores, &old, Sending)??);

        // The key changes (a reinstall): verified becomes unverified, and the new key is not
        // trusted for sending although it is the stored one now; receiving still works.
        let change = ready(stores.identity.save_identity(&remote, new.identity_key()))??;
        assert_eq!(change, IdentityChange::ReplacedExisting);
        let verified: i64 = tx.query_row(
            "SELECT verified FROM identities WHERE service_id = ?1",
            [THEM],
            |r| r.get(0),
        )?;
        assert_eq!(verified, 2, "unverified");
        assert!(!trusted(&stores, &new, Sending)??);
        assert!(!trusted(&stores, &old, Sending)??);
        assert!(trusted(&stores, &new, Receiving)??);
        Ok(())
    })
    .unwrap();
}

#[test]
fn kyber_last_resort_triple_reuse_is_rejected() {
    let (_dir, db) = db();
    let (identity, _) = ours();
    let last_resort = generate_kyber_prekey(&identity, 40).unwrap();
    let one_time = generate_kyber_prekey(&identity, 41).unwrap();
    let base = KeyPair::generate(&mut rand::rng()).public_key;
    let other_base = KeyPair::generate(&mut rand::rng()).public_key;
    db.zone(|tx| -> crate::error::Result<()> {
        insert_kyber_prekey(tx, IdentityKind::Aci, &last_resort, true, true, 0)?;
        insert_kyber_prekey(tx, IdentityKind::Aci, &one_time, false, false, 0)?;
        let mut stores = Stores::new(tx, IdentityKind::Aci, identity, 1, 0);
        let kyber = &mut stores.kyber;

        ready(kyber.mark_kyber_pre_key_used(40.into(), 5.into(), &base))??;
        let reused = ready(kyber.mark_kyber_pre_key_used(40.into(), 5.into(), &base))?;
        assert!(
            matches!(
                reused,
                Err(SignalProtocolError::InvalidMessage(
                    CiphertextMessageType::PreKey,
                    _
                ))
            ),
            "{reused:?}"
        );
        // Another base key, or another signed prekey, is a different triple.
        ready(kyber.mark_kyber_pre_key_used(40.into(), 5.into(), &other_base))??;
        ready(kyber.mark_kyber_pre_key_used(40.into(), 6.into(), &base))??;
        // The last-resort key stays; the one-time key is gone after use.
        assert!(ready(kyber.get_kyber_pre_key(40.into()))?.is_ok());
        ready(kyber.mark_kyber_pre_key_used(41.into(), 5.into(), &base))??;
        assert!(matches!(
            ready(kyber.get_kyber_pre_key(41.into()))?,
            Err(SignalProtocolError::InvalidKyberPreKeyId)
        ));
        assert!(matches!(
            ready(kyber.mark_kyber_pre_key_used(99.into(), 5.into(), &base))?,
            Err(SignalProtocolError::InvalidKyberPreKeyId)
        ));
        Ok(())
    })
    .unwrap();
    // The other identity's store does not see ACI keys.
    db.read(|c| -> crate::error::Result<()> {
        let stores = Stores::new(c, IdentityKind::Pni, identity, 1, 0);
        assert!(ready(stores.kyber.get_kyber_pre_key(40.into()))?.is_err());
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_rolled_back_zone_takes_store_writes_with_it() {
    let (_dir, db) = db();
    let (identity, local) = ours();
    let prekey = PreKeyRecord::new(3.into(), &KeyPair::generate(&mut rand::rng()));
    db.zone(|tx| -> crate::error::Result<()> {
        insert_prekey(tx, IdentityKind::Aci, &prekey, 0)?;
        Ok(())
    })
    .unwrap();
    let them = IdentityKeyPair::generate(&mut rand::rng());
    let result = db.zone(|tx| -> crate::error::Result<()> {
        let mut stores = Stores::new(tx, IdentityKind::Aci, identity, 1, 0);
        ready(stores.prekey.remove_pre_key(3.into()))??;
        let remote = ProtocolAddress::new(THEM.to_owned(), DeviceId::new(1).unwrap());
        ready(process_prekey_bundle(
            &remote,
            &local,
            &mut stores.session,
            &mut stores.identity,
            &bundle(&them, 1),
            SystemTime::now(),
            &mut rand::rng(),
        ))??;
        // Something later in the zone fails (a full disk, a broken invariant).
        Err(Error::Internal("abort"))
    });
    assert!(matches!(result, Err(Error::Internal("abort"))));
    assert!(!has_current_session(&db, 1), "session write rolled back");
    db.read(|c| -> crate::error::Result<()> {
        let stores = Stores::new(c, IdentityKind::Aci, identity, 1, 0);
        assert!(
            ready(stores.prekey.get_pre_key(3.into()))?.is_ok(),
            "prekey still there"
        );
        let remote = ProtocolAddress::new(THEM.to_owned(), DeviceId::new(1).unwrap());
        assert!(
            ready(stores.identity.get_identity(&remote))??.is_none(),
            "identity rolled back"
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn store_failures_are_told_apart_from_protocol_errors() {
    let e = sql_error("op", &kourier_db::rusqlite::Error::InvalidQuery);
    assert!(is_store_failure(&e));
    assert!(!is_store_failure(&SignalProtocolError::InvalidPreKeyId));
    assert!(!is_store_failure(&SignalProtocolError::InvalidState(
        "op",
        "something else".to_owned()
    )));
}
