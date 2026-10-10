//! Our phone's change of number in stage 1 (§6.6, §6.14 item 13; `crate::number_change`): the
//! new PNI identity, keys and registration id are applied in the zone of the batch, envelopes
//! after it in the same batch are already decrypted with them, and nobody but our own account
//! can make us change.

use super::*;

/// What a `PniChangeNumber` from our phone brings.
struct NewNumber {
    pni: Pni,
    identity: IdentityKeyPair,
    change: proto::sync_message::PniChangeNumber,
    /// The bundle a stranger fetches for the new PNI.
    bundle: PreKeyBundle,
}

const NEW_E164: &str = "+15550199";
const NEW_REGISTRATION_ID: u32 = 4242;
const SIGNED_ID: u32 = 50;
const KYBER_ID: u32 = 60;

fn new_number(fixture: &Fixture) -> NewNumber {
    let rng = &mut rand::rng();
    let identity = IdentityKeyPair::generate(rng);
    let signed =
        generate_signed_prekey(&identity, SIGNED_ID, Timestamp::from_epoch_millis(T), rng).unwrap();
    let kyber = generate_kyber_prekey(&identity, KYBER_ID).unwrap();
    let bundle = PreKeyBundle::new(
        NEW_REGISTRATION_ID,
        fixture.account.device_id,
        None,
        signed.id().unwrap(),
        signed.public_key().unwrap(),
        signed.signature().unwrap(),
        kyber.id().unwrap(),
        kyber.public_key().unwrap(),
        kyber.signature().unwrap(),
        *identity.identity_key(),
    )
    .unwrap();
    NewNumber {
        pni: Pni::from(Uuid::new_v4()),
        change: proto::sync_message::PniChangeNumber {
            identity_key_pair: Some(identity.serialize().to_vec()),
            signed_pre_key: Some(signed.serialize().unwrap()),
            last_resort_kyber_pre_key: Some(kyber.serialize().unwrap()),
            registration_id: Some(NEW_REGISTRATION_ID),
            new_e164: Some(NEW_E164.to_owned()),
        },
        identity,
        bundle,
    }
}

/// The sync message of `change` from `phone`, with the envelope's `updatedPni`.
fn change_envelope(
    phone: &mut Phone,
    fixture: &Fixture,
    change: &proto::sync_message::PniChangeNumber,
    updated_pni: Option<Pni>,
) -> Vec<u8> {
    let content = proto::Content {
        content: Some(proto::content::Content::SyncMessage(proto::SyncMessage {
            content: Some(proto::sync_message::Content::PniChangeNumber(
                change.clone(),
            )),
            ..Default::default()
        })),
        ..Default::default()
    };
    let mut envelope =
        proto::Envelope::decode(phone.envelope(fixture, &content, T + 1).as_slice()).unwrap();
    envelope.updated_pni_binary = updated_pni.map(|pni| Uuid::from(pni).as_bytes().to_vec());
    envelope.encode_to_vec()
}

/// A stranger's first message to our *new* number: a PreKey message to the new PNI, made from
/// the new PNI's bundle.
fn to_new_pni(fixture: &Fixture, new: &NewNumber, text: &str) -> (Aci, Vec<u8>) {
    let rng = &mut rand::rng();
    let stranger = Aci::from(Uuid::new_v4());
    let ours = ProtocolAddress::new(
        ServiceId::from(new.pni).service_id_string(),
        fixture.account.device_id,
    );
    let theirs = Phone::address(stranger);
    let mut store = InMemSignalProtocolStore::new(IdentityKeyPair::generate(rng), 79).unwrap();
    ready(process_prekey_bundle(
        &ours,
        &theirs,
        &mut store.session_store,
        &mut store.identity_store,
        &new.bundle,
        SystemTime::now(),
        rng,
    ))
    .unwrap()
    .unwrap();
    let ciphertext = ready(message_encrypt(
        &pad(&text_content(text, T + 2).encode_to_vec()),
        &ours,
        &theirs,
        &mut store.session_store,
        &mut store.identity_store,
        SystemTime::now(),
        rng,
    ))
    .unwrap()
    .unwrap();
    let envelope = proto::Envelope {
        r#type: Some(EnvelopeType::PrekeyMessage.into()),
        source_service_id_binary: Some(ServiceId::from(stranger).service_id_binary()),
        source_device_id: Some(1),
        destination_service_id_binary: Some(ServiceId::from(new.pni).service_id_binary()),
        client_timestamp: Some(T + 2),
        server_timestamp: Some(T + 7),
        content: Some(ciphertext.serialize().to_vec()),
        ..Default::default()
    };
    (stranger, envelope.encode_to_vec())
}

/// Our own phone: device 1 of our account, with our ACI identity.
fn own_phone(fixture: &Fixture) -> Phone {
    Phone::with_identity(fixture, fixture.account.aci, fixture.account.aci_identity)
}

fn batch(fixture: &Fixture, envelopes: &[Vec<u8>]) -> (stage1::Committed, Vec<StatusCode>) {
    let wake = Rc::new(Notify::new());
    let ctx = Stage1 {
        account: &fixture.account,
        trust_roots: &[],
        now: 5_000,
        wake: &wake,
    };
    let sent = Sent::default();
    let committed = stage1::commit_batch(
        &fixture.db,
        &ctx,
        envelopes.iter().map(|b| envelope(b, &sent)).collect(),
    )
    .unwrap();
    let statuses = committed.acks.iter().map(|(_, status)| *status).collect();
    (committed, statuses)
}

fn one<T: kourier_db::rusqlite::types::FromSql>(db: &ZoneDb, sql: &str) -> T {
    db.read(|c| -> crate::error::Result<T> { Ok(c.query_row(sql, [], |r| r.get(0))?) })
        .unwrap()
}

/// Rows that depend on the number: marks, a certificate, a credential, an old PNI session.
fn seed_number_state(fixture: &Fixture) {
    fixture
        .db
        .zone(|tx| -> crate::error::Result<()> {
            tx.execute(
                "INSERT INTO peers(service_id, updated_at, share_my_number) VALUES ('peer', 0, 1)",
                [],
            )?;
            tx.execute(
                "INSERT INTO sender_certs(kind, certificate, expires_at, fetched_at)
                 VALUES ('with_e164', x'00', 1, 1)",
                [],
            )?;
            tx.execute(
                "INSERT INTO group_credentials(redemption_time, credential, fetched_at)
                 VALUES (1, x'00', 1)",
                [],
            )?;
            tx.execute(
                "INSERT INTO sessions(our_kind, their_service_id, device_id, record, updated_at)
                 VALUES ('pni', 'old-friend', 1, x'00', 1)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
}

/// §11 S15: "new PNI identity, keys and registration id applied in one zone", and the
/// envelopes behind it in the same batch already use them.
#[test]
fn a_number_change_from_our_phone_is_applied_before_the_envelopes_behind_it() {
    let fixture = fixture();
    seed_number_state(&fixture);
    let old_pni = fixture.account.pni;
    let mut phone = own_phone(&fixture);
    let mut contact = Phone::new(&fixture);
    let new = new_number(&fixture);
    let (stranger, to_pni) = to_new_pni(&fixture, &new, "to the new number");

    let (committed, statuses) = batch(
        &fixture,
        &[
            change_envelope(&mut phone, &fixture, &new.change, Some(new.pni)),
            to_pni,
            contact.text(&fixture, "to the ACI"),
        ],
    );
    assert_eq!(statuses, [StatusCode::OK; 3]);

    // The account the rest of the actor uses from now on.
    let account = committed.account.expect("the batch changed the account");
    assert_eq!(account.pni, new.pni);
    assert_eq!(account.e164, NEW_E164);
    assert_eq!(account.pni_registration_id, NEW_REGISTRATION_ID);
    assert_eq!(
        account.pni_identity.identity_key(),
        new.identity.identity_key()
    );
    assert_eq!(
        account.aci, fixture.account.aci,
        "the ACI side is untouched"
    );
    assert_eq!(account.aci_registration_id, 11);
    // … and the same in backend.db.
    let stored = fixture.db.read(Account::load).unwrap().expect("account");
    assert_eq!(stored.pni, new.pni);
    assert_eq!(stored.e164, NEW_E164);
    assert_eq!(stored.pni_registration_id, NEW_REGISTRATION_ID);
    assert_eq!(
        stored.pni_identity.identity_key(),
        new.identity.identity_key()
    );
    assert_eq!(
        stored.aci_identity.identity_key(),
        fixture.account.aci_identity.identity_key()
    );

    // The old PNI's keys and sessions are gone; the new signed prekey and last-resort key are
    // there, confirmed (the phone gave them to the server), and the id counters are past them.
    let signed: Vec<(u32, bool)> = fixture
        .db
        .read(|c| -> crate::error::Result<_> {
            let mut stmt =
                c.prepare("SELECT key_id, confirmed FROM signed_prekeys WHERE identity = 'pni'")?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(signed, [(SIGNED_ID, true)]);
    let kyber: Vec<(u32, bool, bool)> = fixture
        .db
        .read(|c| -> crate::error::Result<_> {
            let mut stmt = c.prepare(
                "SELECT key_id, last_resort, confirmed FROM kyber_prekeys WHERE identity = 'pni'",
            )?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(kyber, [(KYBER_ID, true, true)]);
    let ids = fixture
        .db
        .read(|c| KeyIds::load(c, IdentityKind::Pni))
        .unwrap();
    assert_eq!((ids.signed, ids.kyber), (SIGNED_ID + 1, KYBER_ID + 1));
    assert_eq!(
        one::<i64>(
            &fixture.db,
            "SELECT count(*) FROM sessions WHERE their_service_id = 'old-friend'"
        ),
        0,
        "sessions to the old number"
    );
    assert_eq!(
        one::<i64>(
            &fixture.db,
            "SELECT count(*) FROM signed_prekeys WHERE identity = 'aci'"
        ),
        1,
        "ACI keys stay"
    );
    // Our own identities as the identity store sees them: the new PNI, not the old one.
    let identity_rows = |service_id: String| {
        fixture
            .db
            .read(|c| crate::store::load_identity_key(c, &service_id))
            .unwrap()
    };
    assert_eq!(
        identity_rows(ServiceId::from(new.pni).service_id_string()).as_ref(),
        Some(new.identity.identity_key())
    );
    assert_eq!(
        identity_rows(ServiceId::from(old_pni).service_id_string()),
        None
    );

    // What was made for the old number.
    assert_eq!(
        one::<i64>(
            &fixture.db,
            "SELECT share_my_number FROM peers WHERE service_id = 'peer'"
        ),
        0
    );
    assert_eq!(count(&fixture.db, "sender_certs"), 0);
    assert_eq!(count(&fixture.db, "group_credentials"), 0);
    // Fresh PNI prekeys of our own are owed to the server.
    assert_eq!(
        one::<String>(
            &fixture.db,
            "SELECT key FROM jobs WHERE kind = 'key_maintenance'"
        ),
        crate::number_change::FORCED_PNI_KEYS
    );

    // The change itself left no inbox row; the two messages behind it did, the one to the new
    // PNI decrypted with the keys that arrived one envelope earlier.
    let rows: Vec<(String, String)> = fixture
        .db
        .read(|c| -> crate::error::Result<_> {
            let mut stmt = c.prepare("SELECT our_kind, sender FROM inbox ORDER BY arrival")?;
            Ok(stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(
        rows,
        [
            (
                "pni".to_owned(),
                ServiceId::from(stranger).service_id_string()
            ),
            (
                "aci".to_owned(),
                ServiceId::from(contact.aci).service_id_string()
            ),
        ]
    );

    // The core learns our ids and our new number in the same commit.
    let (ready, numbers): (Vec<Vec<UserId>>, Vec<Option<String>>) = outbox_events(&fixture.db)
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::AccountReady {
                own_ids, number, ..
            } => Some((own_ids, number.map(|n| n.expose().clone()))),
            _ => None,
        })
        .unzip();
    assert_eq!(ready, [account.own_ids().unwrap()]);
    assert_eq!(numbers, [Some(NEW_E164.to_owned())]);
    assert_eq!(
        ready[0][1].as_str(),
        format!("pni:{}", Uuid::from(new.pni).hyphenated())
    );
}

/// A number change is our own account's to make, and has to be whole: anything else leaves
/// every row as it was.
#[test]
fn a_number_change_that_is_not_ours_or_not_whole_changes_nothing() {
    let fixture = fixture();
    seed_number_state(&fixture);
    let new = new_number(&fixture);
    let before = fixture.db.read(Account::load).unwrap().expect("account");
    let unchanged = |what: &str| {
        let now = fixture.db.read(Account::load).unwrap().expect("account");
        assert_eq!(now.pni, before.pni, "{what}");
        assert_eq!(now.e164, before.e164, "{what}");
        assert_eq!(
            now.pni_registration_id, before.pni_registration_id,
            "{what}"
        );
        assert_eq!(
            now.pni_identity.identity_key(),
            before.pni_identity.identity_key(),
            "{what}"
        );
        assert_eq!(count(&fixture.db, "jobs"), 0, "{what}");
        assert_eq!(count(&fixture.db, "sender_certs"), 1, "{what}");
        assert_eq!(count(&fixture.db, "core_outbox"), 0, "{what}");
        assert_eq!(count(&fixture.db, "inbox"), 0, "{what}");
    };

    // A contact says our number changed (a hostile sync message).
    let mut contact = Phone::new(&fixture);
    let (committed, statuses) = batch(
        &fixture,
        &[change_envelope(
            &mut contact,
            &fixture,
            &new.change,
            Some(new.pni),
        )],
    );
    assert_eq!(statuses, [StatusCode::OK], "acked, never redelivered");
    assert!(committed.account.is_none());
    unchanged("from a contact");

    // Our phone, but the server named no new PNI on the envelope.
    let mut phone = own_phone(&fixture);
    let (committed, _) = batch(
        &fixture,
        &[change_envelope(&mut phone, &fixture, &new.change, None)],
    );
    assert!(committed.account.is_none());
    unchanged("without the new PNI");

    // Our phone, with keys another identity signed.
    let other = new_number(&fixture);
    let mut forged = new.change.clone();
    forged.signed_pre_key = other.change.signed_pre_key.clone();
    let (committed, _) = batch(
        &fixture,
        &[change_envelope(
            &mut phone,
            &fixture,
            &forged,
            Some(new.pni),
        )],
    );
    assert!(committed.account.is_none());
    unchanged("with a signed prekey of another identity");

    // The whole one is applied after all of that (the session survived the refused ones).
    let (committed, _) = batch(
        &fixture,
        &[change_envelope(
            &mut phone,
            &fixture,
            &new.change,
            Some(new.pni),
        )],
    );
    assert_eq!(committed.account.expect("applied").pni, new.pni);
}
