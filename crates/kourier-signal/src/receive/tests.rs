//! Stage 1 and stage 2 without a server: a libsignal in-memory "phone" encrypts to our stores,
//! stage 1 commits before any ack leaves, a redelivered copy is a duplicate, stage 2 turns the
//! row into a `Message` in `core_outbox` and deletes it.

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use http::StatusCode;
use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::ZoneDb;
use kourier_model::{
    Body, ConvKey, DisappearKind, Expiry, IdentifierKind, IdentityFingerprint, Origin, Patch,
    TrustLevel, UserId,
};
use libsignal_core::{Aci, DeviceId, Pni, ServiceId};
use libsignal_protocol::{
    GenericSignedPreKey as _, IdentityKeyPair, IdentityKeyStore as _, InMemSignalProtocolStore,
    KeyPair, PreKeyBundle, ProtocolAddress, PublicKey, SenderCertificate, ServerCertificate,
    Timestamp, message_encrypt, process_prekey_bundle, sealed_sender_encrypt,
};
use prost::Message as _;
use tokio::sync::Notify;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::stage1::{self, Stage1};
use super::stage2;
use crate::db::{self, Account, KeyIds};
use crate::keys::{generate_kyber_prekey, generate_signed_prekey};
use crate::net::{IncomingEnvelope, ServerAck};
use crate::padding::pad;
use crate::proto::{self, envelope::Type as EnvelopeType};
use crate::store::{IdentityKind, insert_kyber_prekey, insert_signed_prekey, ready};

const T: u64 = 1_700_000_000_123;

pub(crate) struct Fixture {
    _dir: tempfile::TempDir,
    pub(crate) db: Rc<ZoneDb>,
    pub(crate) account: Account,
    bundle: PreKeyBundle,
    pni_bundle: PreKeyBundle,
}

/// A linked account with a signed prekey and a Kyber last-resort key per identity, and their
/// bundles.
pub(crate) fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = Rc::new(
        db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([5; 32]),
        )
        .unwrap(),
    );
    let rng = &mut rand::rng();
    let account = Account {
        aci: Aci::from(Uuid::new_v4()),
        pni: Pni::from(Uuid::new_v4()),
        e164: "+15550100".to_owned(),
        device_id: DeviceId::new(2).unwrap(),
        password: Zeroizing::new("password".to_owned()),
        aci_registration_id: 11,
        pni_registration_id: 12,
        profile_key: Zeroizing::new([9; 32]),
        aci_identity: IdentityKeyPair::generate(rng),
        pni_identity: IdentityKeyPair::generate(rng),
        post_link_done: true,
    };
    let signed = generate_signed_prekey(
        &account.aci_identity,
        1,
        Timestamp::from_epoch_millis(T),
        rng,
    )
    .unwrap();
    let kyber = generate_kyber_prekey(&account.aci_identity, 2).unwrap();
    let pni_signed = generate_signed_prekey(
        &account.pni_identity,
        3,
        Timestamp::from_epoch_millis(T),
        rng,
    )
    .unwrap();
    let pni_kyber = generate_kyber_prekey(&account.pni_identity, 4).unwrap();
    db.zone(|tx| -> crate::error::Result<()> {
        tx.execute(
            "INSERT INTO account(id, aci, pni, e164, device_id, password, aci_registration_id,
               pni_registration_id, profile_key, master_key, storage_key, linked_at)
             VALUES (1, ?1, ?2, ?3, 2, 'password', 11, 12, ?4, ?4, ?4, 0)",
            (
                Uuid::from(account.aci).to_string(),
                Uuid::from(account.pni).to_string(),
                &account.e164,
                &[9u8; 32][..],
            ),
        )?;
        for kind in IdentityKind::ALL {
            db::insert_own_identity(tx, kind, account.identity(kind), KeyIds::random(rng))?;
        }
        insert_signed_prekey(tx, IdentityKind::Aci, &signed, true)?;
        insert_kyber_prekey(tx, IdentityKind::Aci, &kyber, true, true, 0)?;
        insert_signed_prekey(tx, IdentityKind::Pni, &pni_signed, true)?;
        insert_kyber_prekey(tx, IdentityKind::Pni, &pni_kyber, true, true, 0)?;
        Ok(())
    })
    .unwrap();
    let pni_bundle = PreKeyBundle::new(
        12,
        account.device_id,
        None,
        pni_signed.id().unwrap(),
        pni_signed.public_key().unwrap(),
        pni_signed.signature().unwrap(),
        pni_kyber.id().unwrap(),
        pni_kyber.public_key().unwrap(),
        pni_kyber.signature().unwrap(),
        *account.pni_identity.identity_key(),
    )
    .unwrap();
    let bundle = PreKeyBundle::new(
        11,
        account.device_id,
        None,
        signed.id().unwrap(),
        signed.public_key().unwrap(),
        signed.signature().unwrap(),
        kyber.id().unwrap(),
        kyber.public_key().unwrap(),
        kyber.signature().unwrap(),
        *account.aci_identity.identity_key(),
    )
    .unwrap();
    Fixture {
        _dir: dir,
        db,
        account,
        bundle,
        pni_bundle,
    }
}

/// The phone: an in-memory libsignal store with a session to our device.
pub(crate) struct Phone {
    pub(crate) aci: Aci,
    store: InMemSignalProtocolStore,
    /// The session goes to our PNI (`PNI:<uuid>.<device>`) instead of our ACI.
    to_pni: bool,
}

impl Phone {
    pub(crate) fn new(fixture: &Fixture) -> Self {
        Self::with_identity(
            fixture,
            Aci::from(Uuid::new_v4()),
            IdentityKeyPair::generate(&mut rand::rng()),
        )
    }

    /// A phone of `aci` with `identity` (a re-registered phone keeps its ACI, not its key).
    fn with_identity(fixture: &Fixture, aci: Aci, identity: IdentityKeyPair) -> Self {
        let rng = &mut rand::rng();
        let mut store = InMemSignalProtocolStore::new(identity, 77).unwrap();
        ready(process_prekey_bundle(
            &Self::ours(fixture),
            &Self::address(aci),
            &mut store.session_store,
            &mut store.identity_store,
            &fixture.bundle,
            SystemTime::now(),
            rng,
        ))
        .unwrap()
        .unwrap();
        Self {
            aci,
            store,
            to_pni: false,
        }
    }

    /// A phone whose session goes to our PNI, built from our PNI prekeys (the first message
    /// to a number whose ACI the sender does not know).
    fn to_pni(fixture: &Fixture) -> Self {
        let rng = &mut rand::rng();
        let aci = Aci::from(Uuid::new_v4());
        let mut store = InMemSignalProtocolStore::new(IdentityKeyPair::generate(rng), 78).unwrap();
        ready(process_prekey_bundle(
            &fixture.account.address(IdentityKind::Pni),
            &Self::address(aci),
            &mut store.session_store,
            &mut store.identity_store,
            &fixture.pni_bundle,
            SystemTime::now(),
            rng,
        ))
        .unwrap()
        .unwrap();
        Self {
            aci,
            store,
            to_pni: true,
        }
    }

    fn remote(&self, fixture: &Fixture) -> ProtocolAddress {
        if self.to_pni {
            fixture.account.address(IdentityKind::Pni)
        } else {
            Self::ours(fixture)
        }
    }

    fn destination(&self, fixture: &Fixture) -> ServiceId {
        if self.to_pni {
            fixture.account.pni.into()
        } else {
            fixture.account.aci.into()
        }
    }

    /// An unsealed envelope of `content` sent at `timestamp`.
    pub(crate) fn envelope(
        &mut self,
        fixture: &Fixture,
        content: &proto::Content,
        timestamp: u64,
    ) -> Vec<u8> {
        let ciphertext = ready(message_encrypt(
            &pad(&content.encode_to_vec()),
            &self.remote(fixture),
            &Self::address(self.aci),
            &mut self.store.session_store,
            &mut self.store.identity_store,
            SystemTime::now(),
            &mut rand::rng(),
        ))
        .unwrap()
        .unwrap();
        let kind = match ciphertext {
            libsignal_protocol::CiphertextMessage::PreKeySignalMessage(_) => {
                EnvelopeType::PrekeyMessage
            }
            _ => EnvelopeType::DoubleRatchet,
        };
        proto::Envelope {
            r#type: Some(kind.into()),
            source_service_id_binary: Some(ServiceId::from(self.aci).service_id_binary()),
            source_device_id: Some(1),
            destination_service_id_binary: Some(self.destination(fixture).service_id_binary()),
            client_timestamp: Some(timestamp),
            server_timestamp: Some(timestamp + 5),
            content: Some(ciphertext.serialize().to_vec()),
            ..Default::default()
        }
        .encode_to_vec()
    }

    fn ours(fixture: &Fixture) -> ProtocolAddress {
        fixture.account.address(IdentityKind::Aci)
    }

    fn address(aci: Aci) -> ProtocolAddress {
        ProtocolAddress::new(
            ServiceId::from(aci).service_id_string(),
            DeviceId::new(1).unwrap(),
        )
    }

    /// An unsealed envelope with a text DataMessage.
    fn text(&mut self, fixture: &Fixture, text: &str) -> Vec<u8> {
        let content = proto::Content {
            content: Some(proto::content::Content::DataMessage(proto::DataMessage {
                body: Some(text.to_owned()),
                timestamp: Some(T),
                expire_timer: Some(3600),
                ..Default::default()
            })),
            ..Default::default()
        };
        self.envelope(fixture, &content, T)
    }
}

pub(crate) type Sent = Arc<Mutex<Vec<StatusCode>>>;

pub(crate) fn envelope(bytes: &[u8], sent: &Sent) -> IncomingEnvelope {
    let sent = sent.clone();
    IncomingEnvelope {
        bytes: Bytes::copy_from_slice(bytes),
        server_delivery_timestamp: T + 6,
        ack: ServerAck::new(move |status| {
            sent.lock().unwrap().push(status);
            Ok(())
        }),
    }
}

fn count(db: &ZoneDb, table: &str) -> i64 {
    db.read(|c| -> crate::error::Result<i64> {
        Ok(c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    })
    .unwrap()
}

#[test]
fn stage_one_commits_before_acks_and_stage_two_emits_once() {
    let fixture = fixture();
    let mut phone = Phone::new(&fixture);
    let wake = Rc::new(Notify::new());
    let ctx = Stage1 {
        account: &fixture.account,
        trust_roots: &[],
        now: 1,
        wake: &wake,
    };
    let sent = Sent::default();
    let bytes = phone.text(&fixture, "hello");

    let acks = stage1::commit_batch(
        &fixture.db,
        &ctx,
        vec![envelope(&bytes, &sent), envelope(b"\xff\xff", &sent)],
    )
    .unwrap()
    .acks;
    // Committed, and nothing acked yet: acks leave only when the caller sends them.
    assert_eq!(count(&fixture.db, "inbox"), 1);
    // The row keeps the server's delivery time (the age of a call's ring is measured on
    // the server's clock: `calls::age_ms`).
    let delivered_at: Option<i64> = fixture
        .db
        .read(|c| -> crate::error::Result<Option<i64>> {
            Ok(c.query_row("SELECT delivered_at FROM inbox", [], |r| r.get(0))?)
        })
        .unwrap();
    assert_eq!(delivered_at, Some(i64::try_from(T + 6).unwrap()));
    assert!(sent.lock().unwrap().is_empty());
    for (ack, status) in acks {
        ack.respond(status).unwrap();
    }
    assert_eq!(
        *sent.lock().unwrap(),
        [StatusCode::OK, StatusCode::INTERNAL_SERVER_ERROR],
        "the undecodable envelope is answered 500"
    );

    // The server redelivers (no ack reached it): a duplicate, acked, not stored again.
    let acks = stage1::commit_batch(&fixture.db, &ctx, vec![envelope(&bytes, &sent)])
        .unwrap()
        .acks;
    assert_eq!(acks.len(), 1);
    assert_eq!(acks[0].1, StatusCode::OK);
    assert_eq!(count(&fixture.db, "inbox"), 1);

    // Stage 2: the row becomes one Message in core_outbox, in the zone that deletes it.
    let stage2 = || stage2::run_sync(&fixture.db, &fixture.account, Some(0), 2, &wake).unwrap();
    assert_eq!(stage2().0, 1);
    assert_eq!(count(&fixture.db, "inbox"), 0);
    let rows = fixture
        .db
        .read(|c| kourier_db::outbox::peek(c, 10))
        .unwrap();
    assert_eq!(rows.len(), 1);
    let events = without_features(decode_outbox_row(&rows[0].envelope).unwrap());
    let [BackendEvent::Message(message)] = events.as_slice() else {
        panic!("one message: {events:?}");
    };
    let author = crate::service_id::aci_user_id(phone.aci).unwrap();
    assert_eq!(message.conv, ConvKey::Direct(author.clone()));
    assert_eq!(&*message.local, T.to_string());
    assert_eq!(
        message.origin,
        Origin::Remote {
            author,
            device: Some(1)
        }
    );
    assert_eq!(message.body, Body::text("hello"));
    assert_eq!(message.server_at, i64::try_from(T + 5).ok());
    assert_eq!(message.arrival, Some(1));
    assert!(!message.catch_up, "arrival 1 is after boundary 0");
    assert_eq!(
        message.expiry,
        Some(Expiry {
            kind: DisappearKind::AfterRead,
            timer: Duration::from_secs(3600),
            started_at: None,
        }),
        "the sender's disappearing timer, started on read"
    );
    assert_eq!(stage2().0, 0);
}

#[test]
fn pni_and_addressing_rules_drop_without_storing() {
    let fixture = fixture();
    let mut phone = Phone::new(&fixture);
    let wake = Rc::new(Notify::new());
    let ctx = Stage1 {
        account: &fixture.account,
        trust_roots: &[],
        now: 1,
        wake: &wake,
    };
    let sent = Sent::default();
    let valid = proto::Envelope::decode(phone.text(&fixture, "x").as_slice()).unwrap();
    let variant = |edit: &dyn Fn(&mut proto::Envelope)| {
        let mut e = valid.clone();
        edit(&mut e);
        e.encode_to_vec()
    };
    let other_account = variant(&|e| {
        e.destination_service_id_binary =
            Some(ServiceId::from(Aci::from(Uuid::new_v4())).service_id_binary());
    });
    let sealed_to_pni = variant(&|e| {
        e.r#type = Some(EnvelopeType::UnidentifiedSender.into());
        e.destination_service_id_binary =
            Some(ServiceId::from(fixture.account.pni).service_id_binary());
    });
    let ratchet_to_pni = variant(&|e| {
        e.r#type = Some(EnvelopeType::DoubleRatchet.into());
        e.destination_service_id_binary =
            Some(ServiceId::from(fixture.account.pni).service_id_binary());
    });
    let from_a_pni = variant(&|e| {
        e.source_service_id_binary =
            Some(ServiceId::from(Pni::from(Uuid::new_v4())).service_id_binary());
    });
    // Each one is dropped by its rule, before any decryption is attempted (a failed decrypt
    // would also drop it, so the outcome alone would not prove the rule).
    for (bytes, reason) in [
        (&other_account, "not addressed to this account"),
        (&sealed_to_pni, "sealed sender to our PNI"),
        (&ratchet_to_pni, "only prekey messages on our PNI"),
        (&from_a_pni, "message from a PNI"),
    ] {
        let disposition = fixture
            .db
            .zone(|tx| {
                let mut counter = 0;
                stage1::process(
                    tx,
                    &ctx,
                    bytes,
                    None,
                    &mut counter,
                    &mut stage1::Facts::default(),
                )
            })
            .unwrap();
        assert_eq!(disposition, stage1::Disposition::Dropped(reason));
    }
    let acks = stage1::commit_batch(
        &fixture.db,
        &ctx,
        [other_account, sealed_to_pni, ratchet_to_pni, from_a_pni]
            .iter()
            .map(|b| envelope(b, &sent))
            .collect(),
    )
    .unwrap()
    .acks;
    assert!(
        acks.iter().all(|(_, s)| *s == StatusCode::OK),
        "dropped, not rejected"
    );
    assert_eq!(count(&fixture.db, "inbox"), 0);
    // None of them touched the arrival counter.
    assert_eq!(fixture.db.read(db::arrival_counter).unwrap(), 0);
}

impl Phone {
    /// A sealed-sender envelope with a text, under a sender certificate that names `sender`
    /// (ACI, device), is signed by a server certificate under `root` and expires at `expires`.
    fn sealed(
        &mut self,
        fixture: &Fixture,
        root: &KeyPair,
        sender: (Aci, u32),
        expires: u64,
        server_timestamp: Option<u64>,
    ) -> Vec<u8> {
        self.sealed_from(fixture, root, sender, expires, server_timestamp, None, T)
    }

    /// [`Self::sealed`] with the sender's number in the certificate and the message sent at
    /// `timestamp`.
    #[allow(clippy::too_many_arguments, reason = "test helper")]
    fn sealed_from(
        &mut self,
        fixture: &Fixture,
        root: &KeyPair,
        sender: (Aci, u32),
        expires: u64,
        server_timestamp: Option<u64>,
        e164: Option<&str>,
        timestamp: u64,
    ) -> Vec<u8> {
        let rng = &mut rand::rng();
        let server = KeyPair::generate(rng);
        let server_cert =
            ServerCertificate::new(1, server.public_key, &root.private_key, rng).unwrap();
        let identity = ready(self.store.identity_store.get_identity_key_pair())
            .unwrap()
            .unwrap();
        let certificate = SenderCertificate::new(
            Uuid::from(sender.0).to_string(),
            e164.map(str::to_owned),
            *identity.public_key(),
            DeviceId::new(u8::try_from(sender.1).unwrap()).unwrap(),
            Timestamp::from_epoch_millis(expires),
            server_cert,
            &server.private_key,
            rng,
        )
        .unwrap();
        let content = proto::Content {
            content: Some(proto::content::Content::DataMessage(proto::DataMessage {
                body: Some("sealed".to_owned()),
                timestamp: Some(timestamp),
                ..Default::default()
            })),
            ..Default::default()
        };
        let sealed = ready(sealed_sender_encrypt(
            &Self::ours(fixture),
            &certificate,
            &pad(&content.encode_to_vec()),
            &mut self.store.session_store,
            &mut self.store.identity_store,
            SystemTime::now(),
            rng,
        ))
        .unwrap()
        .unwrap();
        proto::Envelope {
            r#type: Some(EnvelopeType::UnidentifiedSender.into()),
            destination_service_id_binary: Some(
                ServiceId::from(fixture.account.aci).service_id_binary(),
            ),
            client_timestamp: Some(timestamp),
            server_timestamp,
            content: Some(sealed),
            ..Default::default()
        }
        .encode_to_vec()
    }
}

#[test]
fn sealed_sender_certificates_are_validated_before_decrypting() {
    let fixture = fixture();
    let mut phone = Phone::new(&fixture);
    let rng = &mut rand::rng();
    let root = KeyPair::generate(rng);
    let other_root = KeyPair::generate(rng);
    let wake = Rc::new(Notify::new());
    let trust_roots = [root.public_key];
    let ctx = Stage1 {
        account: &fixture.account,
        trust_roots: &trust_roots,
        now: 1,
        wake: &wake,
    };
    let day = 86_400_000;
    let from_phone = (phone.aci, 1);
    let ours = (fixture.account.aci, u32::from(fixture.account.device_id));
    let rejected = [
        // Signed under a root this environment does not trust.
        phone.sealed(&fixture, &other_root, from_phone, T + day, Some(T + 5)),
        // Expired before the server received it.
        phone.sealed(&fixture, &root, from_phone, T, Some(T + 5)),
        // Without a server timestamp the certificate cannot be checked.
        phone.sealed(&fixture, &root, from_phone, T + day, None),
        // Claims to come from this very device.
        phone.sealed(&fixture, &root, ours, T + day, Some(T + 5)),
    ];
    let sent = Sent::default();
    let acks = stage1::commit_batch(
        &fixture.db,
        &ctx,
        rejected.iter().map(|b| envelope(b, &sent)).collect(),
    )
    .unwrap()
    .acks;
    assert!(
        acks.iter().all(|(_, s)| *s == StatusCode::OK),
        "dropped and acked, never redelivered"
    );
    assert_eq!(count(&fixture.db, "inbox"), 0);
    assert_eq!(count(&fixture.db, "sessions"), 0, "nothing decrypted");

    // A valid certificate: stored under the sender the certificate names.
    let valid = phone.sealed(&fixture, &root, from_phone, T + day, Some(T + 5));
    let acks = stage1::commit_batch(&fixture.db, &ctx, vec![envelope(&valid, &sent)])
        .unwrap()
        .acks;
    assert_eq!(acks[0].1, StatusCode::OK);
    let (sender, device, sealed): (String, u32, bool) = fixture
        .db
        .read(|c| -> crate::error::Result<_> {
            Ok(
                c.query_row("SELECT sender, sender_device, sealed FROM inbox", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?,
            )
        })
        .unwrap();
    assert_eq!(sender, ServiceId::from(phone.aci).service_id_string());
    assert_eq!(device, 1);
    assert!(sealed);
}

// ---------------------------------------------------------------------------------------------
// S7: facts in stage 1, PNI, plaintext and delivery receipts, content mapping, poison, lanes
// ---------------------------------------------------------------------------------------------

/// A text `Content` sent at `timestamp`.
pub(crate) fn text_content(text: &str, timestamp: u64) -> proto::Content {
    proto::Content {
        content: Some(proto::content::Content::DataMessage(proto::DataMessage {
            body: Some(text.to_owned()),
            timestamp: Some(timestamp),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Stage 1 over `envelopes` with the acks sent; returns their statuses.
fn stage1_all(
    fixture: &Fixture,
    trust_roots: &[PublicKey],
    envelopes: &[Vec<u8>],
) -> Vec<StatusCode> {
    let wake = Rc::new(Notify::new());
    let ctx = Stage1 {
        account: &fixture.account,
        trust_roots,
        now: 1_000,
        wake: &wake,
    };
    let sent = Sent::default();
    let acks = stage1::commit_batch(
        &fixture.db,
        &ctx,
        envelopes.iter().map(|b| envelope(b, &sent)).collect(),
    )
    .unwrap()
    .acks;
    for (ack, status) in acks {
        ack.respond(status).unwrap();
    }
    sent.lock().unwrap().clone()
}

/// Every event in `core_outbox`, in order.
fn outbox_events(db: &ZoneDb) -> Vec<BackendEvent> {
    without_features(
        db.read(|c| kourier_db::outbox::peek(c, 10_000))
            .unwrap()
            .iter()
            .flat_map(|row| decode_outbox_row(&row.envelope).unwrap())
            .collect(),
    )
}

/// `events` without the conversation-feature upserts stage 2 puts in front of the first event
/// of a chat (`crate::features`, tested there).
pub(crate) fn without_features(events: Vec<BackendEvent>) -> Vec<BackendEvent> {
    events
        .into_iter()
        .filter(|event| {
            !matches!(event, BackendEvent::ConversationUpsert(patch) if patch.features.is_some())
        })
        .collect()
}

fn clear_outbox(db: &ZoneDb) {
    db.zone(|tx| -> crate::error::Result<()> {
        tx.execute("DELETE FROM core_outbox", [])?;
        Ok(())
    })
    .unwrap();
}

/// Stage 2 over every due row with a known boundary (nothing is catch-up).
fn stage2_all(fixture: &Fixture, now: i64) -> Vec<BackendEvent> {
    let wake = Rc::new(Notify::new());
    stage2::run_sync(
        &fixture.db,
        &fixture.account,
        Some(u64::MAX >> 1),
        now,
        &wake,
    )
    .unwrap()
    .1
}

fn messages(events: &[BackendEvent]) -> Vec<&kourier_backend_api::IncomingMessage> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Message(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn aci_user(aci: Aci) -> UserId {
    crate::service_id::aci_user_id(aci).unwrap()
}

#[test]
fn an_identity_change_is_recorded_in_the_stage_one_commit() {
    let fixture = fixture();
    let mut phone = Phone::new(&fixture);
    let first = phone.text(&fixture, "before");
    assert_eq!(stage1_all(&fixture, &[], &[first]), [StatusCode::OK]);
    assert!(
        outbox_events(&fixture.db).is_empty(),
        "first use is no change"
    );

    // The contact re-registered: the same ACI with a new identity key, a new session.
    let identity = IdentityKeyPair::generate(&mut rand::rng());
    let mut again = Phone::with_identity(&fixture, phone.aci, identity);
    let bytes = again.envelope(&fixture, &text_content("after", T + 10), T + 10);
    assert_eq!(stage1_all(&fixture, &[], &[bytes]), [StatusCode::OK]);
    // In core_outbox together with the inbox row, before stage 2 ran.
    assert_eq!(count(&fixture.db, "inbox"), 2);
    let events = outbox_events(&fixture.db);
    let [BackendEvent::ContactUpsert(patch)] = events.as_slice() else {
        panic!("one contact change: {events:?}");
    };
    assert_eq!(patch.user, aci_user(phone.aci));
    assert_eq!(patch.trust, Some(TrustLevel::Unverified));
    assert_eq!(
        patch.identity_fingerprint,
        Patch::Set(IdentityFingerprint(
            identity.identity_key().serialize().into_vec()
        ))
    );
    // Both messages still arrive.
    clear_outbox(&fixture.db);
    let bodies: Vec<_> = messages(&stage2_all(&fixture, 2_000))
        .iter()
        .map(|m| m.body.clone())
        .collect();
    assert!(outbox_events(&fixture.db).len() == 2, "events of both rows");
    assert!(
        bodies.is_empty(),
        "stage 2 events go to core_outbox, not the return value"
    );
}

/// Desktop's `addKeyChange`: a changed identity key leaves a notice in the 1:1 chat and in
/// every group the contact is a member of, but only where the core has the conversation; the
/// notice's id comes from the key and the conversation, so the same change reported twice is
/// one row.
#[test]
fn an_identity_change_leaves_a_notice_where_the_core_has_the_conversation() {
    use crate::groups::{GroupKeys, SnapMember, Snapshot};
    use crate::store::IdentityChangeRecord;
    use kourier_model::{Body, ConvKey, Origin, SystemEvent};

    let fixture = fixture();
    let alice = Aci::from(Uuid::from_u128(0xa11ce));
    let sid = ServiceId::from(alice).service_id_string();
    let user = aci_user(alice);
    let change = |identity: &IdentityKeyPair| IdentityChangeRecord {
        service_id: sid.clone(),
        identity: *identity.identity_key(),
    };
    let first = IdentityKeyPair::generate(&mut rand::rng());
    let second = IdentityKeyPair::generate(&mut rand::rng());
    let with_alice = GroupKeys::from_master([4; 32]);
    let without = GroupKeys::from_master([5; 32]);
    let notices = |events: &[BackendEvent]| -> Vec<(ConvKey, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::Message(m) => {
                    assert_eq!(
                        m.origin,
                        Origin::Remote {
                            author: user.clone(),
                            device: None
                        }
                    );
                    assert_eq!(
                        m.body,
                        Body::System(SystemEvent::IdentityKeyChanged { user: user.clone() })
                    );
                    assert!(m.silent, "a notice never notifies");
                    Some((m.conv.clone(), m.local.to_string()))
                }
                _ => None,
            })
            .collect()
    };
    fixture
        .db
        .zone(|tx| -> crate::error::Result<()> {
            // Nobody the core knows: the contact change alone.
            let events = stage1::identity_events(tx, &[change(&first)], 50)?;
            assert!(matches!(
                events.as_slice(),
                [BackendEvent::ContactUpsert(_)]
            ));

            crate::peers::set_features_hash(tx, &sid, 1, 1)?;
            for (keys, members) in [(&with_alice, vec![alice]), (&without, vec![])] {
                let snapshot = Snapshot {
                    revision: 1,
                    members: members
                        .into_iter()
                        .map(|aci| SnapMember {
                            aci: aci.into(),
                            admin: false,
                            joined_at: 0,
                        })
                        .collect(),
                    ..Snapshot::default()
                };
                tx.execute(
                    "INSERT INTO groups(master_key, group_id, updated_at, revision, state,
                       membership, features_hash) VALUES (?1, ?2, 0, 1, ?3, 'member', 42)",
                    (&keys.master[..], &keys.id[..], snapshot.to_json()?),
                )?;
            }
            let events = stage1::identity_events(tx, &[change(&first)], 60)?;
            assert!(matches!(events[0], BackendEvent::ContactUpsert(_)));
            let found = notices(&events);
            assert_eq!(found.len(), 2, "the 1:1 chat and her group: {found:?}");
            assert_eq!(found[0].0, ConvKey::Direct(user.clone()));
            assert!(found[0].1.starts_with("keychange:") && found[0].1.len() == 26);
            assert_eq!(found[1].0, with_alice.conv_key());
            assert!(found[1].1.starts_with(&found[0].1), "{found:?}");
            assert_ne!(
                found[1].1, found[0].1,
                "ids are unique per account and author"
            );

            // Reported again (decrypt, then a prekey fetch): the same rows.
            assert_eq!(
                notices(&stage1::identity_events(tx, &[change(&first)], 70)?),
                found
            );
            // Another key is another change.
            let again = notices(&stage1::identity_events(tx, &[change(&second)], 80)?);
            assert_ne!(again[0].1, found[0].1);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_sealed_senders_number_becomes_a_contact_identifier_once() {
    let fixture = fixture();
    let mut phone = Phone::new(&fixture);
    let root = KeyPair::generate(&mut rand::rng());
    let trust_roots = [root.public_key];
    let day = 86_400_000;
    let sender = (phone.aci, 1);
    let mut sealed = |e164: Option<&str>, t: u64| {
        phone.sealed_from(&fixture, &root, sender, T + day, Some(t + 5), e164, t)
    };
    let batch = [
        sealed(Some("+15550111"), T),
        sealed(Some("+15550111"), T + 1),
        sealed(None, T + 2),
        sealed(Some("12345"), T + 3),
    ];
    assert!(
        stage1_all(&fixture, &trust_roots, &batch)
            .iter()
            .all(|s| *s == StatusCode::OK)
    );
    let numbers = |events: &[BackendEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::ContactUpsert(patch) => {
                    assert_eq!(patch.user, aci_user(phone_aci(&fixture, &batch)));
                    let ids = patch.identifiers.as_ref()?;
                    assert_eq!(ids.len(), 1);
                    assert_eq!(ids[0].kind, IdentifierKind::Phone);
                    Some(ids[0].value.clone())
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        numbers(&outbox_events(&fixture.db)),
        ["+15550111"],
        "once, valid only"
    );
    assert_eq!(count(&fixture.db, "inbox"), 4);

    // A new number is sent again.
    clear_outbox(&fixture.db);
    let changed = sealed(Some("+15550122"), T + 4);
    stage1_all(&fixture, &trust_roots, &[changed]);
    assert_eq!(numbers(&outbox_events(&fixture.db)), ["+15550122"]);
    let stored: String = fixture
        .db
        .read(|c| -> crate::error::Result<String> {
            Ok(c.query_row("SELECT e164 FROM peers", [], |r| r.get(0))?)
        })
        .unwrap();
    assert_eq!(stored, "+15550122");
}

/// The sender of the stored inbox rows (one sender in these tests).
fn phone_aci(fixture: &Fixture, _batch: &[Vec<u8>]) -> Aci {
    let sender: String = fixture
        .db
        .read(|c| -> crate::error::Result<String> {
            Ok(c.query_row("SELECT sender FROM inbox LIMIT 1", [], |r| r.get(0))?)
        })
        .unwrap();
    match ServiceId::parse_from_service_id_string(&sender) {
        Some(ServiceId::Aci(aci)) => aci,
        other => panic!("not an ACI: {other:?}"),
    }
}

#[test]
fn a_prekey_message_to_our_pni_is_received_and_remembers_that_they_know_our_number() {
    let fixture = fixture();
    let mut phone = Phone::to_pni(&fixture);
    let bytes = phone.envelope(&fixture, &text_content("to my number", T), T);
    let envelope = proto::Envelope::decode(bytes.as_slice()).unwrap();
    assert_eq!(envelope.r#type(), EnvelopeType::PrekeyMessage);
    assert_eq!(stage1_all(&fixture, &[], &[bytes]), [StatusCode::OK]);
    let (our_kind, share): (String, i64) = fixture
        .db
        .read(|c| -> crate::error::Result<_> {
            let our_kind = c.query_row("SELECT our_kind FROM inbox", [], |r| r.get(0))?;
            let share = c.query_row(
                "SELECT share_my_number FROM peers WHERE service_id = ?1",
                [ServiceId::from(phone.aci).service_id_string()],
                |r| r.get(0),
            )?;
            Ok((our_kind, share))
        })
        .unwrap();
    assert_eq!(our_kind, "pni");
    assert_eq!(share, 1);
    stage2_all(&fixture, 2_000);
    let events = outbox_events(&fixture.db);
    let [message] = messages(&events)[..] else {
        panic!("one message: {events:?}");
    };
    assert_eq!(message.body, Body::text("to my number"));
    assert_eq!(message.conv, ConvKey::Direct(aci_user(phone.aci)));
}

#[test]
fn a_pni_signature_merges_the_pni_into_the_aci_only_when_it_verifies() {
    let fixture = fixture();
    let mut phone = Phone::new(&fixture);
    let rng = &mut rand::rng();
    let pni = Pni::from(Uuid::new_v4());
    let pni_identity = IdentityKeyPair::generate(rng);
    let aci_identity = ready(phone.store.identity_store.get_identity_key_pair())
        .unwrap()
        .unwrap();
    let signed = |by: &IdentityKeyPair, t: u64| {
        let mut content = text_content("hi", t);
        content.pni_signature_message = Some(proto::PniSignatureMessage {
            pni: Some(Uuid::from(pni).as_bytes().to_vec()),
            signature: Some(
                by.sign_alternate_identity(aci_identity.identity_key(), &mut rand::rng())
                    .unwrap()
                    .to_vec(),
            ),
        });
        content
    };
    // Their PNI's identity key is unknown: nothing to verify against, nothing merges.
    let first = phone.envelope(&fixture, &signed(&pni_identity, T), T);
    stage1_all(&fixture, &[], &[first]);
    assert!(outbox_events(&fixture.db).is_empty());

    // Once it is known (a prekey fetch for their number), a valid signature merges.
    fixture
        .db
        .zone(|tx| {
            crate::store::save_own_identity(
                tx,
                &ServiceId::from(pni).service_id_string(),
                pni_identity.identity_key(),
                1,
            )
            .map_err(crate::error::Error::from)
        })
        .unwrap();
    let forged = phone.envelope(
        &fixture,
        &signed(&IdentityKeyPair::generate(rng), T + 1),
        T + 1,
    );
    let valid = phone.envelope(&fixture, &signed(&pni_identity, T + 2), T + 2);
    stage1_all(&fixture, &[], &[forged, valid]);
    assert_eq!(
        outbox_events(&fixture.db),
        [BackendEvent::IdentityMerged {
            from: crate::service_id::pni_user_id(pni).unwrap(),
            into: aci_user(phone.aci),
        }],
        "only the valid signature merges"
    );
    assert_eq!(
        count(&fixture.db, "inbox"),
        3,
        "the messages themselves are kept"
    );

    // A signature in unencrypted content (`PLAINTEXT_CONTENT`) proves nothing about the sender
    // (Desktop's `wasEncrypted`): plaintext carries nothing but retry requests, so the envelope
    // is acked and dropped, and nothing merges.
    clear_outbox(&fixture.db);
    let mut plaintext = vec![0xc0];
    plaintext.extend(pad(&signed(&pni_identity, T + 3).encode_to_vec()));
    let unencrypted = proto::Envelope {
        r#type: Some(EnvelopeType::PlaintextContent.into()),
        source_service_id_binary: Some(ServiceId::from(phone.aci).service_id_binary()),
        source_device_id: Some(1),
        client_timestamp: Some(T + 3),
        server_timestamp: Some(T + 4),
        content: Some(plaintext),
        ..Default::default()
    }
    .encode_to_vec();
    assert_eq!(stage1_all(&fixture, &[], &[unencrypted]), [StatusCode::OK]);
    assert_eq!(count(&fixture.db, "inbox"), 3);
    assert!(
        outbox_events(&fixture.db).is_empty(),
        "no merge from unencrypted content"
    );
}

#[test]
fn plaintext_content_and_server_delivery_receipts_are_routed() {
    use libsignal_protocol::{CiphertextMessageType, DecryptionErrorMessage, PlaintextContent};
    let fixture = fixture();
    let mut phone = Phone::new(&fixture);
    // A decryption-error message about one of our messages, in PlaintextContent.
    let original = proto::Envelope::decode(phone.text(&fixture, "x").as_slice()).unwrap();
    let error = DecryptionErrorMessage::for_original(
        original.content.as_deref().unwrap(),
        CiphertextMessageType::PreKey,
        Timestamp::from_epoch_millis(T),
        2,
    )
    .unwrap();
    let plaintext = proto::Envelope {
        r#type: Some(EnvelopeType::PlaintextContent.into()),
        source_service_id_binary: Some(ServiceId::from(phone.aci).service_id_binary()),
        source_device_id: Some(1),
        client_timestamp: Some(T + 1),
        server_timestamp: Some(T + 2),
        content: Some(PlaintextContent::from(error).serialized().to_vec()),
        ..Default::default()
    }
    .encode_to_vec();
    let pni_source = Pni::from(Uuid::new_v4());
    let receipt = |source: ServiceId| {
        proto::Envelope {
            r#type: Some(EnvelopeType::ServerDeliveryReceipt.into()),
            source_service_id_binary: Some(source.service_id_binary()),
            source_device_id: Some(3),
            client_timestamp: Some(T - 100),
            server_timestamp: Some(T + 9),
            ..Default::default()
        }
        .encode_to_vec()
    };
    let statuses = stage1_all(
        &fixture,
        &[],
        &[
            plaintext,
            receipt(phone.aci.into()),
            receipt(pni_source.into()),
        ],
    );
    assert!(statuses.iter().all(|s| *s == StatusCode::OK));
    let lanes: Vec<String> = fixture
        .db
        .read(|c| -> crate::error::Result<_> {
            let mut stmt = c.prepare("SELECT lane FROM inbox ORDER BY arrival")?;
            let lanes = stmt
                .query_map([], |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<Vec<String>>>()?;
            Ok(lanes)
        })
        .unwrap();
    assert_eq!(lanes, ["misc", "misc", "misc"]);
    stage2_all(&fixture, 2_000);
    assert_eq!(count(&fixture.db, "inbox"), 0);
    let receipts: Vec<_> = outbox_events(&fixture.db)
        .into_iter()
        .map(|e| match e {
            BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            } => {
                assert_eq!(targets.len(), 1);
                assert_eq!(targets[0].author, kourier_model::TargetAuthor::Own);
                assert_eq!(&*targets[0].local, (T - 100).to_string());
                assert_eq!(kind, kourier_model::ReceiptKind::Delivered);
                assert_eq!(at, i64::try_from(T + 9).unwrap());
                by
            }
            other => panic!("only receipts: {other:?}"),
        })
        .collect();
    assert_eq!(
        receipts,
        [
            aci_user(phone.aci),
            crate::service_id::pni_user_id(pni_source).unwrap()
        ],
        "the decryption error waits for retry requests; receipts from an ACI and a PNI"
    );
}

// -- content mapping (dispatch) ---------------------------------------------------------------

mod mapping {
    use kourier_model::{
        CallDirection, CallLine, CallMedia, CallState, Patch, RangeKind, ReceiptKind,
        RecipientState, SystemEvent, TargetAuthor, TextRange,
    };

    use super::*;
    use crate::groups::{GroupKeys, SnapMember, Snapshot};
    use crate::receive::dispatch::{Ctx, Outcome, dispatch};
    use crate::receive::stage2::Row;
    use crate::receive::{KIND_CONTENT, MISC_LANE};

    const PEER: &str = "3e6a63a4-6d94-46f0-9ab4-8a35c8f4f5a1";

    /// The wall clock of the mapping tests: a little after every message they map.
    const NOW: i64 = T as i64 + 60_000;

    fn peer() -> Aci {
        Aci::from(Uuid::parse_str(PEER).unwrap())
    }

    fn row(sender: ServiceId, device: u32, timestamp: u64, content: &proto::Content) -> Row {
        Row {
            arrival: 7,
            received_at: 100,
            kind: KIND_CONTENT.to_owned(),
            sender: sender.service_id_string(),
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

    fn map(fixture: &Fixture, row: &Row, catch_up: bool) -> Outcome {
        fixture
            .db
            .zone(|tx| {
                dispatch(
                    tx,
                    &Ctx {
                        account: &fixture.account,
                        catch_up,
                        now: NOW,
                        long_text: None,
                    },
                    row,
                )
            })
            .map(|outcome| match outcome {
                Outcome::Handled {
                    events,
                    ephemeral,
                    note,
                } => Outcome::Handled {
                    events: without_features(events),
                    ephemeral,
                    note,
                },
                failed => failed,
            })
            .unwrap()
    }

    fn events_of(outcome: Outcome) -> Vec<BackendEvent> {
        match outcome {
            Outcome::Handled { events, .. } => events,
            failed => panic!("not handled: {failed:?}"),
        }
    }

    /// Turns the typing-indicator setting on (the configuration sync's job).
    fn typing_on(fixture: &Fixture) {
        fixture
            .db
            .zone(|tx| crate::db::store_configuration(tx, None, Some(true)))
            .unwrap();
    }

    fn data(message: proto::DataMessage) -> proto::Content {
        proto::Content {
            content: Some(proto::content::Content::DataMessage(message)),
            ..Default::default()
        }
    }

    fn sync(message: proto::SyncMessage) -> proto::Content {
        proto::Content {
            content: Some(proto::content::Content::SyncMessage(message)),
            ..Default::default()
        }
    }

    fn style(start: u32, length: u32, style: proto::body_range::Style) -> proto::BodyRange {
        proto::BodyRange {
            start: Some(start),
            length: Some(length),
            associated_value: Some(proto::body_range::AssociatedValue::Style(style.into())),
        }
    }

    #[test]
    fn a_text_keeps_its_ranges_mentions_quote_and_timer() {
        let fixture = fixture();
        let me = fixture.account.aci;
        // "Hi ￼ 😀bold": the mention is UTF-16 [3, 1), "bold" [7, 11); in UTF-8 [3, 6) and
        // [11, 15).
        let message = proto::DataMessage {
            body: Some("Hi \u{FFFC} \u{1F600}bold".to_owned()),
            timestamp: Some(T),
            expire_timer: Some(60),
            body_ranges: vec![
                proto::BodyRange {
                    start: Some(3),
                    length: Some(1),
                    associated_value: Some(proto::body_range::AssociatedValue::MentionAci(
                        Uuid::from(me).to_string(),
                    )),
                },
                style(7, 4, proto::body_range::Style::Bold),
                style(40, 4, proto::body_range::Style::Italic),
            ],
            quote: Some(proto::data_message::Quote {
                id: Some(42),
                author_aci: Some(Uuid::from(me).to_string()),
                text: Some("original".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let events = events_of(map(
            &fixture,
            &row(peer().into(), 2, T, &data(message)),
            true,
        ));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        let Body::Text(text) = &m.body else {
            panic!("text: {:?}", m.body);
        };
        assert_eq!(
            text.ranges,
            [
                TextRange {
                    start: 3,
                    len: 3,
                    kind: RangeKind::Mention(aci_user(me)),
                },
                TextRange {
                    start: 11,
                    len: 4,
                    kind: RangeKind::Bold,
                },
            ],
            "UTF-16 → UTF-8, the out-of-range one dropped"
        );
        assert!(m.mentions_me);
        assert!(m.catch_up);
        assert_eq!(m.conv, ConvKey::Direct(aci_user(peer())));
        assert_eq!(m.arrival, Some(7));
        assert_eq!(m.expiry.map(|e| e.timer), Some(Duration::from_secs(60)));
        let reply = m.reply_to.as_ref().unwrap();
        assert_eq!(reply.target.author, TargetAuthor::Own);
        assert_eq!(&*reply.target.local, "42");
        assert_eq!(reply.author, None);
        assert_eq!(
            reply.snapshot.as_ref().and_then(|s| s.text.clone()),
            Some(kourier_model::Text::plain("original"))
        );
    }

    #[test]
    fn reactions_deletes_and_edits_become_modifiers() {
        let fixture = fixture();
        let me = fixture.account.aci;
        let reaction = proto::DataMessage {
            timestamp: Some(T),
            reaction: Some(proto::data_message::Reaction {
                emoji: Some("👍".to_owned()),
                target_author_aci_binary: Some(Uuid::from(me).as_bytes().to_vec()),
                target_sent_timestamp: Some(41),
                ..Default::default()
            }),
            ..Default::default()
        };
        let events = events_of(map(
            &fixture,
            &row(peer().into(), 1, T, &data(reaction)),
            false,
        ));
        let [
            BackendEvent::Reaction {
                target,
                by,
                emoji,
                remove,
                at,
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(target.author, TargetAuthor::Own);
        assert_eq!(&*target.local, "41");
        assert_eq!(by.as_ref(), Some(&aci_user(peer())));
        assert_eq!((emoji.as_str(), *remove), ("👍", false));
        assert_eq!(*at, i64::try_from(T + 5).unwrap(), "dated by the server");

        // A "reaction" that is not one emoji (a word, a link, two emoji) is dropped, as on
        // Desktop: no contact can pin text under somebody's message.
        for not_an_emoji in ["lol", "https://example.org", "👍👍", "👍 "] {
            let reaction = proto::DataMessage {
                timestamp: Some(T),
                reaction: Some(proto::data_message::Reaction {
                    emoji: Some(not_an_emoji.to_owned()),
                    target_author_aci_binary: Some(Uuid::from(me).as_bytes().to_vec()),
                    target_sent_timestamp: Some(41),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let outcome = map(&fixture, &row(peer().into(), 1, T, &data(reaction)), false);
            assert!(
                matches!(&outcome, Outcome::Handled { events, note: Some(_), .. } if events.is_empty()),
                "{not_an_emoji:?}: {outcome:?}"
            );
        }

        let delete = proto::DataMessage {
            timestamp: Some(T),
            delete: Some(proto::data_message::Delete {
                target_sent_timestamp: Some(40),
            }),
            ..Default::default()
        };
        let events = events_of(map(
            &fixture,
            &row(peer().into(), 1, T, &data(delete)),
            false,
        ));
        let [
            BackendEvent::Deleted {
                target,
                for_everyone: true,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(target.author, TargetAuthor::User(aci_user(peer())));
        assert_eq!(&*target.local, "40");

        let edit = proto::Content {
            content: Some(proto::content::Content::EditMessage(proto::EditMessage {
                target_sent_timestamp: Some(39),
                data_message: Some(proto::DataMessage {
                    body: Some("fixed".to_owned()),
                    timestamp: Some(T + 1),
                    ..Default::default()
                }),
            })),
            ..Default::default()
        };
        let events = events_of(map(&fixture, &row(peer().into(), 1, T + 1, &edit), false));
        let [
            BackendEvent::Edited {
                target,
                by,
                new_local,
                body,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(&*target.local, "39");
        assert_eq!(by.as_ref(), Some(&aci_user(peer())));
        assert_eq!(new_local.as_deref(), Some(&*(T + 1).to_string()));
        assert_eq!(body, &Body::text("fixed"));
    }

    #[test]
    fn timer_updates_follow_the_version_rules() {
        let fixture = fixture();
        let timer = |secs: u32, version: Option<u32>, t: u64| {
            data(proto::DataMessage {
                timestamp: Some(t),
                flags: Some(2),
                expire_timer: Some(secs),
                expire_timer_version: version,
                ..Default::default()
            })
        };
        let changed = |outcome: Outcome| -> Option<Option<Duration>> {
            let events = events_of(outcome);
            match events.as_slice() {
                [] => None,
                [
                    BackendEvent::ConversationUpsert(patch),
                    BackendEvent::Message(m),
                ] => {
                    let Body::System(SystemEvent::TimerChanged { by, timer }) = &m.body else {
                        panic!("{:?}", m.body);
                    };
                    assert_eq!(by.as_ref(), Some(&aci_user(peer())));
                    assert!(
                        patch.timer_version.is_some(),
                        "the patch states the version"
                    );
                    match (&patch.timer, timer) {
                        (Patch::Set(a), Some(b)) => assert_eq!(a, b),
                        (Patch::Clear, None) => {}
                        other => panic!("{other:?}"),
                    }
                    Some(*timer)
                }
                other => panic!("{other:?}"),
            }
        };
        let hour = Some(Duration::from_secs(3600));
        let sender: ServiceId = peer().into();
        assert_eq!(
            changed(map(
                &fixture,
                &row(sender, 1, T, &timer(3600, Some(2), T)),
                false
            )),
            Some(hour)
        );
        assert_eq!(
            changed(map(
                &fixture,
                &row(sender, 1, T + 1, &timer(60, Some(1), T + 1)),
                false
            )),
            None,
            "an older version is ignored"
        );
        assert_eq!(
            changed(map(
                &fixture,
                &row(sender, 1, T + 2, &timer(0, Some(3), T + 2)),
                false
            )),
            Some(None),
            "a newer version turns it off"
        );
        assert_eq!(
            changed(map(
                &fixture,
                &row(sender, 1, T + 3, &timer(0, Some(4), T + 3)),
                false
            )),
            None,
            "the timer it already has: nothing to show"
        );
        // An update without a version (a client from before timer versions): Desktop and the
        // phones apply its timer; our version (4 by now) stays.
        let outcome = map(
            &fixture,
            &row(sender, 1, T + 4, &timer(3600, None, T + 4)),
            false,
        );
        let events = events_of(outcome);
        let [
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(notice),
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(patch.timer, Patch::Set(Duration::from_secs(3600)));
        assert_eq!(patch.timer_version, Some(4), "our version is kept");
        assert_eq!(&*notice.local, (T + 4).to_string());
        let stored = fixture
            .db
            .read(|c| crate::peers::load(c, &sender.service_id_string()))
            .unwrap();
        assert_eq!(
            (stored.expire_timer, stored.expire_timer_version),
            (3600, 4)
        );
        // The next one with a version still counts from there.
        assert_eq!(
            changed(map(
                &fixture,
                &row(sender, 1, T + 5, &timer(60, Some(3), T + 5)),
                false
            )),
            None,
            "still stale"
        );
    }

    /// Desktop's `handleDataMessage`: an ordinary 1:1 message states its chat's timer too. One
    /// that differs changes the chat (version rules as for an update), with a notice of its
    /// own id above the message; the message keeps its own expiry either way.
    #[test]
    fn an_ordinary_message_brings_the_chats_timer() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let text = |secs: Option<u32>, version: Option<u32>, t: u64| {
            data(proto::DataMessage {
                body: Some("hi".to_owned()),
                timestamp: Some(t),
                expire_timer: secs,
                expire_timer_version: version,
                ..Default::default()
            })
        };
        let user = aci_user(peer());
        // No version: the message expires by its own timer, the chat's timer stays.
        let events = events_of(map(
            &fixture,
            &row(sender, 1, T, &text(Some(60), None, T)),
            false,
        ));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert_eq!(m.expiry.map(|e| e.timer), Some(Duration::from_secs(60)));

        // With a version: patch, notice, message, in this order.
        let events = events_of(map(
            &fixture,
            &row(sender, 1, T + 1, &text(Some(60), Some(2), T + 1)),
            false,
        ));
        let [
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(notice),
            BackendEvent::Message(m),
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(patch.timer, Patch::Set(Duration::from_secs(60)));
        assert_eq!(patch.timer_version, Some(2));
        assert_eq!(&*notice.local, format!("timer:{}", T + 1));
        assert_eq!(
            notice.sent_at,
            i64::try_from(T).unwrap(),
            "one ms above the message"
        );
        assert_eq!(
            notice.body,
            Body::System(SystemEvent::TimerChanged {
                by: Some(user.clone()),
                timer: Some(Duration::from_secs(60)),
            })
        );
        assert_eq!(&*m.local, (T + 1).to_string());
        assert_eq!(m.body, Body::text("hi"));

        // The same timer again: just the message. A reaction never touches the timer.
        let events = events_of(map(
            &fixture,
            &row(sender, 1, T + 2, &text(Some(60), Some(2), T + 2)),
            false,
        ));
        assert!(
            matches!(events.as_slice(), [BackendEvent::Message(_)]),
            "{events:?}"
        );
        let reaction = data(proto::DataMessage {
            timestamp: Some(T + 3),
            expire_timer: Some(0),
            expire_timer_version: Some(9),
            reaction: Some(proto::data_message::Reaction {
                emoji: Some("👍".to_owned()),
                target_author_aci_binary: Some(Uuid::from(peer()).as_bytes().to_vec()),
                target_sent_timestamp: Some(T + 1),
                ..Default::default()
            }),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(sender, 1, T + 3, &reaction), false));
        assert!(
            matches!(events.as_slice(), [BackendEvent::Reaction { .. }]),
            "{events:?}"
        );

        // A message without a timer and a newer version turns it off.
        let events = events_of(map(
            &fixture,
            &row(sender, 1, T + 4, &text(None, Some(3), T + 4)),
            false,
        ));
        let [
            BackendEvent::ConversationUpsert(patch),
            BackendEvent::Message(_),
            BackendEvent::Message(_),
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(patch.timer, Patch::Clear);
        let stored = fixture
            .db
            .read(|c| crate::peers::load(c, &sender.service_id_string()))
            .unwrap();
        assert_eq!((stored.expire_timer, stored.expire_timer_version), (0, 3));
    }

    #[test]
    fn end_session_archives_the_sessions_and_leaves_a_notice() {
        let fixture = fixture();
        let mut phone = Phone::new(&fixture);
        let first = phone.text(&fixture, "x");
        stage1_all(&fixture, &[], &[first]);
        let usable = || {
            fixture
                .db
                .read(|c| {
                    crate::store::session_devices(
                        c,
                        IdentityKind::Aci,
                        &ServiceId::from(phone.aci).service_id_string(),
                    )
                    .map_err(crate::error::Error::from)
                })
                .unwrap()
                .iter()
                .any(|(_, record)| {
                    record
                        .has_usable_sender_chain(
                            SystemTime::now(),
                            libsignal_protocol::SessionUsabilityRequirements::empty(),
                        )
                        .unwrap()
                })
        };
        assert!(usable());
        let end = data(proto::DataMessage {
            timestamp: Some(T + 1),
            flags: Some(1),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(phone.aci.into(), 1, T + 1, &end), false));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert_eq!(
            m.body,
            Body::System(SystemEvent::SessionRefreshed {
                user: aci_user(phone.aci)
            })
        );
        assert!(!usable(), "archived");
    }

    /// S12: attachments with the body as caption, voice notes, stickers, and long texts (the
    /// whole text when the lane downloaded it, else the trimmed body).
    #[test]
    fn attachments_stickers_and_long_texts_map_to_media() {
        use kourier_model::{AttachmentKind, AttachmentState};
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let pointer = |mime: &str, flags: u32| proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "cdn-key".to_owned(),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            digest: Some(vec![2; 32]),
            size: Some(4096),
            content_type: Some(mime.to_owned()),
            flags: Some(flags),
            width: Some(640),
            height: Some(480),
            ..Default::default()
        };
        let media = data(proto::DataMessage {
            body: Some("a *photo*".to_owned()),
            timestamp: Some(T),
            attachments: vec![pointer("image/jpeg", 0), pointer("audio/aac", 1)],
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(sender, 1, T, &media), false));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        let Body::Media {
            attachments,
            caption,
        } = &m.body
        else {
            panic!("{:?}", m.body);
        };
        assert_eq!(caption.as_ref().map(|c| c.body.as_str()), Some("a *photo*"));
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].kind, AttachmentKind::Image);
        assert_eq!(attachments[0].dims, Some((640, 480)));
        assert_eq!(attachments[0].state, AttachmentState::Remote);
        assert_eq!(attachments[1].kind, AttachmentKind::Voice);
        assert!(m.raw.is_none(), "media is shown, not kept raw");

        let sticker = data(proto::DataMessage {
            timestamp: Some(T + 1),
            sticker: Some(proto::data_message::Sticker {
                pack_id: Some(vec![9; 16]),
                pack_key: Some(vec![8; 32]),
                sticker_id: Some(3),
                data: Some(pointer("image/webp", 0)),
                emoji: Some("x".to_owned()),
            }),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(sender, 1, T + 1, &sticker), false));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(
            matches!(&m.body, Body::Sticker(a) if a.kind == AttachmentKind::Image && a.mime == "image/webp"),
            "{:?}",
            m.body
        );

        let long = format!("{} end", "word ".repeat(1000));
        let trimmed = &long[..2048];
        let long_text = data(proto::DataMessage {
            body: Some(trimmed.to_owned()),
            timestamp: Some(T + 2),
            attachments: vec![pointer(crate::send::media::LONG_TEXT_MIME, 0)],
            ..Default::default()
        });
        let long_row = row(sender, 1, T + 2, &long_text);
        let body_with = |text: Option<&str>| {
            let outcome = fixture
                .db
                .zone(|tx| {
                    dispatch(
                        tx,
                        &Ctx {
                            account: &fixture.account,
                            catch_up: false,
                            now: 1,
                            long_text: text,
                        },
                        &long_row,
                    )
                })
                .unwrap();
            match events_of(outcome).as_slice() {
                [BackendEvent::Message(m)] => m.body.clone(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(body_with(Some(&long)), Body::text(long.as_str()));
        // The download failed: the trimmed body, and the rest stays reachable as a file.
        let Body::Media {
            attachments,
            caption,
        } = body_with(None)
        else {
            panic!("the long-text attachment is kept");
        };
        assert_eq!(caption.map(|c| c.body), Some(trimmed.to_owned()));
        assert_eq!(attachments.len(), 1);
        assert_eq!(
            (attachments[0].kind, attachments[0].mime.as_str()),
            (AttachmentKind::File, crate::send::media::LONG_TEXT_MIME)
        );
        assert_eq!(attachments[0].state, AttachmentState::Remote);
    }

    /// Edits and attachments: an edited long text arrives whole or is tried again, never cut to
    /// its first 2 KiB; the new caption of a media message is not applied as a text revision
    /// (that would replace the pictures).
    #[test]
    fn edits_keep_long_texts_whole_and_media_edits_are_captions() {
        let fixture = fixture();
        let pointer = |mime: &str, size: u32| proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "cdn-key".to_owned(),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            digest: Some(vec![2; 32]),
            size: Some(size),
            content_type: Some(mime.to_owned()),
            ..Default::default()
        };
        let long = format!("{} end", "word ".repeat(1000));
        let trimmed = &long[..2048];
        let edit_row = |body: &str, attachments: Vec<proto::AttachmentPointer>| {
            let edit = proto::Content {
                content: Some(proto::content::Content::EditMessage(proto::EditMessage {
                    target_sent_timestamp: Some(39),
                    data_message: Some(proto::DataMessage {
                        body: Some(body.to_owned()),
                        timestamp: Some(T + 1),
                        attachments,
                        ..Default::default()
                    }),
                })),
                ..Default::default()
            };
            row(peer().into(), 1, T + 1, &edit)
        };
        let outcome = |row: &Row, long_text: Option<&str>| {
            fixture
                .db
                .zone(|tx| {
                    dispatch(
                        tx,
                        &Ctx {
                            account: &fixture.account,
                            catch_up: false,
                            now: 1,
                            long_text,
                        },
                        row,
                    )
                })
                .unwrap()
        };
        let edited_body = |outcome: Outcome| {
            events_of(outcome)
                .into_iter()
                .find_map(|event| match event {
                    BackendEvent::Edited { body, .. } => Some(body),
                    _ => None,
                })
        };
        let size = u32::try_from(long.len()).unwrap();
        let long_edit = edit_row(
            trimmed,
            vec![pointer(crate::send::media::LONG_TEXT_MIME, size)],
        );
        assert_eq!(
            edited_body(outcome(&long_edit, Some(&long))),
            Some(Body::text(long.as_str())),
            "the whole text"
        );
        // Not downloaded (yet): the row fails and is tried again, without a placeholder.
        assert!(matches!(
            outcome(&long_edit, None),
            Outcome::Failed { visible: false, .. }
        ));
        // A long-text pointer nothing can be fetched from: the trimmed body is all there is.
        let broken = proto::AttachmentPointer {
            key: None,
            ..pointer(crate::send::media::LONG_TEXT_MIME, size)
        };
        assert_eq!(
            edited_body(outcome(&edit_row(trimmed, vec![broken]), None)),
            Some(Body::text(trimmed))
        );
        // A new caption for a photo: the edit names the caption, and the photo as the sender
        // listed it (the core keeps the message's own files, whatever the edit lists).
        let caption = edit_row("a better caption", vec![pointer("image/jpeg", 4096)]);
        let Some(Body::Media {
            attachments,
            caption: Some(text),
        }) = edited_body(outcome(&caption, None))
        else {
            panic!("a media revision");
        };
        assert_eq!(text.body, "a better caption");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, kourier_model::AttachmentKind::Image);
        // The caption removed: a media revision without one (never an empty text revision,
        // which would empty a text message).
        let removed = edit_row("", vec![pointer("image/jpeg", 4096)]);
        assert!(matches!(
            edited_body(outcome(&removed, None)),
            Some(Body::Media { caption: None, .. })
        ));
        // A long caption with its photo: the whole text once downloaded, the long-text
        // attachment not among the files.
        let long_caption = edit_row(
            trimmed,
            vec![
                pointer(crate::send::media::LONG_TEXT_MIME, size),
                pointer("image/jpeg", 4096),
            ],
        );
        let Some(Body::Media {
            attachments,
            caption: Some(text),
        }) = edited_body(outcome(&long_caption, Some(&long)))
        else {
            panic!("a media revision");
        };
        assert_eq!(text.body, long);
        assert_eq!(attachments.len(), 1);
        // The edit is dated by the server (the receive window must not trust the sender).
        let at = events_of(outcome(&caption, None))
            .into_iter()
            .find_map(|event| match event {
                BackendEvent::Edited { edited_at, .. } => Some(edited_at),
                _ => None,
            });
        assert_eq!(
            at,
            Some(i64::try_from(T + 1).unwrap() + 5),
            "the row's server timestamp"
        );
    }

    /// View-once media is not mapped as media (the core would download it and keep it in the
    /// timeline for good) and nothing of it is kept: no attachment, no pointer, no raw message.
    /// The same holds for our phone's transcript of one we sent.
    #[test]
    fn view_once_media_is_never_mapped_or_kept() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let message = proto::DataMessage {
            body: Some("for your eyes only".to_owned()),
            timestamp: Some(T),
            is_view_once: Some(true),
            attachments: vec![proto::AttachmentPointer {
                attachment_identifier: Some(
                    proto::attachment_pointer::AttachmentIdentifier::CdnKey("cdn-key".to_owned()),
                ),
                cdn_number: Some(3),
                key: Some(vec![1; 64]),
                digest: Some(vec![2; 32]),
                size: Some(4096),
                content_type: Some("image/jpeg".to_owned()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let incoming = data(message.clone());
        let transcript = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Sent(
                proto::sync_message::Sent {
                    destination_service_id_binary: Some(sender.service_id_binary()),
                    timestamp: Some(T),
                    message: Some(message),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        let own: ServiceId = fixture.account.aci.into();
        for (what, row) in [
            ("received", row(sender, 1, T, &incoming)),
            ("our phone's transcript", row(own, 1, T, &transcript)),
        ] {
            let events = events_of(map(&fixture, &row, false));
            let message = events
                .iter()
                .find_map(|event| match event {
                    BackendEvent::Message(m) => Some(m),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{what}: {events:?}"));
            assert!(
                matches!(&message.body, Body::Unsupported { kind_hint, .. } if kind_hint == "view-once"),
                "{what}: {:?}",
                message.body
            );
            assert!(message.view_once, "{what}");
            assert!(message.raw.is_none(), "{what}: the pointer is not kept");
        }
    }

    #[test]
    fn unsupported_content_is_kept_raw_and_side_effects_apply() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let shared_contact = data(proto::DataMessage {
            body: Some("caption".to_owned()),
            timestamp: Some(T),
            contact: vec![proto::data_message::Contact::default()],
            profile_key: Some(vec![7; 32]),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(sender, 1, T, &shared_contact), false));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!(
            matches!(&m.body, Body::Unsupported { kind_hint, .. } if kind_hint == "contact"),
            "{:?}",
            m.body
        );
        assert_eq!(m.raw.as_ref().map(|r| r.v), Some(1));
        let key: Vec<u8> = fixture
            .db
            .read(|c| -> crate::error::Result<_> {
                Ok(c.query_row("SELECT profile_key FROM peers", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(key, [7; 32], "the profile key is stored");

        for (content, why) in [
            (
                data(proto::DataMessage {
                    timestamp: Some(T),
                    flags: Some(4),
                    profile_key: Some(vec![8; 32]),
                    ..Default::default()
                }),
                "profile key update",
            ),
            (
                data(proto::DataMessage {
                    body: Some("group".to_owned()),
                    timestamp: Some(T),
                    group_v2: Some(proto::GroupContextV2 {
                        master_key: Some(vec![1; 32]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                "group message",
            ),
            (
                proto::Content {
                    content: Some(proto::content::Content::NullMessage(
                        proto::NullMessage::default(),
                    )),
                    ..Default::default()
                },
                "null message",
            ),
        ] {
            assert!(
                events_of(map(&fixture, &row(sender, 1, T, &content), false)).is_empty(),
                "{why}"
            );
        }
    }

    /// Content Desktop shows and Kourier does not is never dropped silently: each kind gets a
    /// placeholder with a `kind_hint` of its own and its raw message; modifiers of such
    /// placeholders (a poll vote, an unpin) change nothing here and are dropped.
    #[test]
    fn content_kourier_does_not_show_becomes_a_placeholder_of_its_kind() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let target = Some(Uuid::from(peer()).as_bytes().to_vec());
        let cases: Vec<(&str, proto::DataMessage)> = vec![
            (
                "payment",
                proto::DataMessage {
                    payment: Some(proto::data_message::Payment::default()),
                    ..Default::default()
                },
            ),
            (
                "gift-badge",
                proto::DataMessage {
                    gift_badge: Some(proto::data_message::GiftBadge::default()),
                    ..Default::default()
                },
            ),
            (
                "poll",
                proto::DataMessage {
                    poll_create: Some(proto::data_message::PollCreate {
                        question: Some("Lunch?".to_owned()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            (
                "poll-ended",
                proto::DataMessage {
                    poll_terminate: Some(proto::data_message::PollTerminate {
                        target_sent_timestamp: Some(5),
                    }),
                    ..Default::default()
                },
            ),
            (
                "story-reply",
                proto::DataMessage {
                    body: Some("nice".to_owned()),
                    story_context: Some(proto::data_message::StoryContext::default()),
                    ..Default::default()
                },
            ),
            (
                "pin",
                proto::DataMessage {
                    pin_message: Some(proto::data_message::PinMessage {
                        target_author_aci_binary: target.clone(),
                        target_sent_timestamp: Some(5),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
        ];
        for (i, (kind, mut message)) in (0u64..).zip(cases) {
            let t = T + i;
            message.timestamp = Some(t);
            let events = events_of(map(&fixture, &row(sender, 1, t, &data(message)), false));
            let [BackendEvent::Message(m)] = events.as_slice() else {
                panic!("{kind}: {events:?}");
            };
            assert!(
                matches!(&m.body, Body::Unsupported { kind_hint, .. } if kind_hint == kind),
                "{kind}: {:?}",
                m.body
            );
            assert_eq!(m.raw.as_ref().map(|r| r.v), Some(1), "{kind}: kept raw");
            assert!(!m.view_once);
        }
        for (why, message) in [
            (
                // Outside a group it means nothing: no "Call" bubble, and no call line.
                "group-call update in a 1:1 chat",
                proto::DataMessage {
                    timestamp: Some(T + 50),
                    group_call_update: Some(proto::data_message::GroupCallUpdate {
                        era_id: Some("era".to_owned()),
                    }),
                    ..Default::default()
                },
            ),
            (
                "poll vote",
                proto::DataMessage {
                    timestamp: Some(T + 50),
                    poll_vote: Some(proto::data_message::PollVote::default()),
                    ..Default::default()
                },
            ),
            (
                "unpin",
                proto::DataMessage {
                    timestamp: Some(T + 50),
                    unpin_message: Some(proto::data_message::UnpinMessage {
                        target_author_aci_binary: target.clone(),
                        target_sent_timestamp: Some(5),
                    }),
                    ..Default::default()
                },
            ),
            (
                "admin delete outside a group",
                proto::DataMessage {
                    timestamp: Some(T + 50),
                    admin_delete: Some(proto::data_message::AdminDelete {
                        target_author_aci_binary: target.clone(),
                        target_sent_timestamp: Some(5),
                    }),
                    ..Default::default()
                },
            ),
        ] {
            let outcome = map(&fixture, &row(sender, 1, T + 50, &data(message)), false);
            assert!(
                matches!(&outcome, Outcome::Handled { events, note: Some(_), .. } if events.is_empty()),
                "{why}: {outcome:?}"
            );
        }
    }

    /// A reaction to a story names the story, which is no message of the chat: Desktop shows
    /// it as a message of its own ("Reacted to your story"), so it becomes a story-reply
    /// placeholder, from the contact and from our phone alike, never a reaction parked for a
    /// target that does not exist.
    #[test]
    fn a_reaction_to_a_story_is_a_placeholder_not_a_reaction() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let me: ServiceId = fixture.account.aci.into();
        let story_reaction = |t: u64| proto::DataMessage {
            timestamp: Some(t),
            reaction: Some(proto::data_message::Reaction {
                emoji: Some("🔥".to_owned()),
                remove: Some(false),
                target_author_aci: None,
                target_author_aci_binary: Some(Uuid::from(fixture.account.aci).as_bytes().to_vec()),
                target_sent_timestamp: Some(5),
            }),
            story_context: Some(proto::data_message::StoryContext {
                author_aci: None,
                author_aci_binary: Some(Uuid::from(fixture.account.aci).as_bytes().to_vec()),
                sent_timestamp: Some(5),
            }),
            ..Default::default()
        };
        let is_story_reply = |events: &[BackendEvent]| {
            let [BackendEvent::Message(m)] = events else {
                panic!("{events:?}");
            };
            assert!(
                matches!(&m.body, Body::Unsupported { kind_hint, .. } if kind_hint == "story-reply"),
                "{:?}",
                m.body
            );
            assert_eq!(m.raw.as_ref().map(|r| r.v), Some(1), "kept raw");
        };
        let events = events_of(map(
            &fixture,
            &row(sender, 1, T, &data(story_reaction(T))),
            false,
        ));
        is_story_reply(&events);
        let transcript = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Sent(
                proto::sync_message::Sent {
                    destination_service_id_binary: Some(sender.service_id_binary()),
                    timestamp: Some(T + 1),
                    message: Some(story_reaction(T + 1)),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(me, 1, T + 1, &transcript), false));
        is_story_reply(&events[..1]);

        // The same reaction without a story is a reaction.
        let mut plain = story_reaction(T + 2);
        plain.story_context = None;
        let events = events_of(map(&fixture, &row(sender, 1, T + 2, &data(plain)), false));
        assert!(
            matches!(events.as_slice(), [BackendEvent::Reaction { .. }]),
            "{events:?}"
        );
    }

    /// A reply our phone sent, and an edit of it from there: the backend keeps the quote under
    /// each id, because an edit Kourier sends of that message must carry it (receivers drop the
    /// quote of an edit that has none, `SD/ts/util/handleEditMessage.preload.ts`).
    #[test]
    fn the_quotes_of_replies_from_our_phone_are_kept_for_their_edits() {
        use crate::send::outgoing::stored_quote;
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let quote = proto::data_message::Quote {
            id: Some(5),
            author_aci_binary: Some(Uuid::from(peer()).as_bytes().to_vec()),
            text: Some("the question".to_owned()),
            ..Default::default()
        };
        let reply =
            |t: u64, body: &str, quote: Option<&proto::data_message::Quote>| proto::DataMessage {
                timestamp: Some(t),
                body: Some(body.to_owned()),
                quote: quote.cloned(),
                ..Default::default()
            };
        let sent = |t: u64, sent: proto::sync_message::Sent| {
            sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::Sent(
                    proto::sync_message::Sent {
                        destination_service_id_binary: Some(
                            ServiceId::from(peer()).service_id_binary(),
                        ),
                        timestamp: Some(t),
                        ..sent
                    },
                )),
                ..Default::default()
            })
        };
        let kept = |local: u64| {
            fixture
                .db
                .read(|c| stored_quote(c, &local.to_string()))
                .unwrap()
        };
        let transcript = sent(
            T,
            proto::sync_message::Sent {
                message: Some(reply(T, "the answer", Some(&quote))),
                ..Default::default()
            },
        );
        let events = events_of(map(&fixture, &row(me, 1, T, &transcript), false));
        assert!(messages(&events)[0].reply_to.is_some());
        assert_eq!(kept(T), Some(quote.clone()), "the reply's quote");

        // Its edit from the phone: the next edit targets this revision.
        let edit = sent(
            T + 1,
            proto::sync_message::Sent {
                edit_message: Some(proto::EditMessage {
                    target_sent_timestamp: Some(T),
                    data_message: Some(reply(T + 1, "the better answer", Some(&quote))),
                }),
                ..Default::default()
            },
        );
        let events = events_of(map(&fixture, &row(me, 1, T + 1, &edit), false));
        assert!(matches!(events.as_slice(), [BackendEvent::Edited { .. }]));
        assert_eq!(kept(T + 1), Some(quote.clone()), "the edit's quote");

        // A message of ours without a quote keeps nothing, and nobody else's quote is kept.
        let plain = sent(
            T + 2,
            proto::sync_message::Sent {
                message: Some(reply(T + 2, "no reply", None)),
                ..Default::default()
            },
        );
        events_of(map(&fixture, &row(me, 1, T + 2, &plain), false));
        assert_eq!(kept(T + 2), None);
        let theirs = data(reply(T + 3, "their reply", Some(&quote)));
        events_of(map(&fixture, &row(peer().into(), 1, T + 3, &theirs), false));
        assert_eq!(kept(T + 3), None, "only our own replies");
    }

    /// `PROFILE_KEY_UPDATE`: the key is stored (a new access key, sealed sender unknown again)
    /// and the profile is fetched again; nothing shows in the timeline.
    #[test]
    fn a_profile_key_update_stores_the_key_and_refetches_the_profile() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let update = |key: u8, t: u64| {
            data(proto::DataMessage {
                timestamp: Some(t),
                flags: Some(4),
                profile_key: Some(vec![key; 32]),
                ..Default::default()
            })
        };
        let stored = || -> (Vec<u8>, i64) {
            fixture
                .db
                .read(|c| -> crate::error::Result<_> {
                    Ok((
                        c.query_row("SELECT profile_key FROM peers", [], |r| r.get(0))?,
                        crate::jobs::count(c, crate::jobs::JobKind::ProfileFetch)?,
                    ))
                })
                .unwrap()
        };
        assert!(events_of(map(&fixture, &row(sender, 1, T, &update(8, T)), false)).is_empty());
        assert_eq!(
            stored(),
            (vec![8; 32], 1),
            "stored, and a profile fetch queued"
        );
        let access = fixture
            .db
            .read(|c| crate::peers::load(c, &sender.service_id_string()))
            .unwrap();
        assert_eq!(
            access.access_key,
            Some(crate::peers::derive_access_key(&[8; 32]).unwrap())
        );
        // A new key replaces it.
        assert!(
            events_of(map(
                &fixture,
                &row(sender, 1, T + 1, &update(9, T + 1)),
                false
            ))
            .is_empty()
        );
        assert_eq!(stored().0, vec![9; 32]);
    }

    /// A quote names the quoted attachment (`Quote.QuotedAttachment`): its kind from the content
    /// type, its name and type; the thumbnail is never fetched.
    #[test]
    fn quotes_name_the_quoted_attachment() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let reply = |attachments: Vec<proto::data_message::quote::QuotedAttachment>| {
            data(proto::DataMessage {
                body: Some("look".to_owned()),
                timestamp: Some(T),
                quote: Some(proto::data_message::Quote {
                    id: Some(41),
                    author_aci_binary: Some(Uuid::from(fixture.account.aci).as_bytes().to_vec()),
                    text: Some("the caption".to_owned()),
                    attachments,
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        let quoted = |content: proto::Content| {
            let events = events_of(map(&fixture, &row(sender, 1, T, &content), false));
            let [BackendEvent::Message(m)] = events.as_slice() else {
                panic!("{events:?}");
            };
            m.reply_to.clone().expect("a quote")
        };
        let photo = quoted(reply(vec![proto::data_message::quote::QuotedAttachment {
            content_type: Some("image/png".to_owned()),
            file_name: Some("cat.png".to_owned()),
            thumbnail: Some(proto::AttachmentPointer::default()),
        }]));
        assert_eq!(photo.target.author, TargetAuthor::Own);
        assert_eq!(&*photo.target.local, "41");
        assert_eq!(photo.author, None, "our own message");
        let snapshot = photo.snapshot.expect("snapshot");
        assert_eq!(
            snapshot.attachment,
            Some(kourier_model::AttachmentKind::Image)
        );
        assert_eq!(snapshot.mime.as_deref(), Some("image/png"));
        assert_eq!(snapshot.file_name.as_deref(), Some("cat.png"));
        assert_eq!(
            snapshot.text.map(|t| t.body),
            Some("the caption".to_owned())
        );
        assert_eq!(snapshot.thumb, None, "a backend never sets the thumbnail");

        let voice = quoted(reply(vec![proto::data_message::quote::QuotedAttachment {
            content_type: Some("audio/aac".to_owned()),
            ..Default::default()
        }]));
        assert_eq!(
            voice.snapshot.and_then(|s| s.attachment),
            Some(kourier_model::AttachmentKind::Audio)
        );
        let text_only = quoted(reply(Vec::new()));
        let snapshot = text_only.snapshot.expect("snapshot");
        assert_eq!((snapshot.attachment, snapshot.mime), (None, None));
    }

    /// A long text the lane could not download stays with the message as the rest of its text
    /// (`Attachment::long_text`), so the core fetches and joins it; other files are files.
    #[test]
    fn a_long_text_that_was_not_downloaded_is_marked_for_the_core() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let pointer = |mime: &str| proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "cdn-key".to_owned(),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            digest: Some(vec![2; 32]),
            size: Some(5000),
            content_type: Some(mime.to_owned()),
            ..Default::default()
        };
        let message = data(proto::DataMessage {
            body: Some("the first 2 KiB".to_owned()),
            timestamp: Some(T),
            attachments: vec![
                pointer(crate::send::media::LONG_TEXT_MIME),
                pointer("image/jpeg"),
            ],
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(sender, 1, T, &message), false));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        let Body::Media {
            attachments,
            caption,
        } = &m.body
        else {
            panic!("{:?}", m.body);
        };
        assert_eq!(
            caption.as_ref().map(|c| c.body.as_str()),
            Some("the first 2 KiB")
        );
        let flags: Vec<bool> = attachments.iter().map(|a| a.long_text).collect();
        assert_eq!(flags, [true, false]);
        assert!(attachments[0].remote.is_some(), "the core can download it");
    }

    /// `SyncMessage.attachmentBackfillResponse`: only the primary's (device 1 of our ACI) is
    /// news for the core, named WITH its conversation (binary or string ids); one from another
    /// of our devices, one naming its chat or author by number alone, an empty one and every
    /// request (only the primary answers) are dropped with a reason.
    #[test]
    fn backfill_answers_come_only_from_the_primary() {
        use kourier_backend_api::{RestoreSlot, RestoreUpdate};
        use proto::addressable_message::Author;
        use proto::conversation_identifier::Identifier;
        use proto::sync_message::AttachmentBackfillResponse as Response;
        use proto::sync_message::attachment_backfill_response::{
            AttachmentData, AttachmentDataList, Data,
        };
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let them: ServiceId = peer().into();
        let answer = |author: Author, thread: Identifier, data: Option<Data>| {
            sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::AttachmentBackfillResponse(
                    Response {
                        target_message: Some(proto::AddressableMessage {
                            author: Some(author),
                            sent_timestamp: Some(10),
                        }),
                        target_conversation: Some(proto::ConversationIdentifier {
                            identifier: Some(thread),
                        }),
                        data,
                    },
                )),
                ..Default::default()
            })
        };
        let pending = || {
            Some(Data::Attachments(AttachmentDataList {
                attachments: vec![AttachmentData {
                    data: Some(
                        proto::sync_message::attachment_backfill_response::attachment_data::Data::Status(0),
                    ),
                }],
                long_text: None,
            }))
        };
        let binary = answer(
            Author::AuthorServiceIdBinary(them.service_id_binary()),
            Identifier::ThreadServiceIdBinary(them.service_id_binary()),
            pending(),
        );
        let chat = Some(ConvKey::Direct(aci_user(peer())));
        match &events_of(map(&fixture, &row(me, 1, T, &binary), false))[..] {
            [BackendEvent::AttachmentRestore { target, update, .. }] => {
                assert_eq!(target.conv, chat);
                assert_eq!(target.author, TargetAuthor::User(aci_user(peer())));
                assert_eq!(target.local.to_string(), "10");
                assert_eq!(
                    *update,
                    RestoreUpdate::Answer {
                        attachments: vec![RestoreSlot::Pending],
                        long_text: None
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        // Desktop's string forms; our own message is `Own`; `error` = 0 is "not found".
        let strings = answer(
            Author::AuthorServiceId(me.service_id_string()),
            Identifier::ThreadServiceId(them.service_id_string()),
            Some(Data::Error(0)),
        );
        match &events_of(map(&fixture, &row(me, 1, T, &strings), false))[..] {
            [BackendEvent::AttachmentRestore { target, update, .. }] => {
                assert_eq!((&target.conv, &target.author), (&chat, &TargetAuthor::Own));
                assert_eq!(*update, RestoreUpdate::MessageNotFound);
            }
            other => panic!("{other:?}"),
        }
        let dropped = |content: &proto::Content, device: u32| {
            matches!(
                map(&fixture, &row(me, device, T, content), false),
                Outcome::Handled { events, note: Some(_), .. } if events.is_empty()
            )
        };
        // Device 2 is this device in the fixture (its own sync is dropped before): another
        // linked device is 3.
        assert!(dropped(&binary, 3), "a linked device does not answer");
        assert!(dropped(
            &answer(
                Author::AuthorE164("+15550100".to_owned()),
                Identifier::ThreadServiceIdBinary(them.service_id_binary()),
                pending()
            ),
            1
        ));
        assert!(dropped(
            &answer(
                Author::AuthorServiceIdBinary(them.service_id_binary()),
                Identifier::ThreadE164("+15550100".to_owned()),
                pending()
            ),
            1
        ));
        assert!(dropped(
            &answer(
                Author::AuthorServiceIdBinary(them.service_id_binary()),
                Identifier::ThreadServiceIdBinary(them.service_id_binary()),
                None
            ),
            1
        ));
        let request = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::AttachmentBackfillRequest(
                proto::sync_message::AttachmentBackfillRequest::default(),
            )),
            ..Default::default()
        });
        assert!(dropped(&request, 1), "only the primary answers a request");
        // From anyone but our own account it is not a sync message at all.
        assert!(events_of(map(&fixture, &row(them, 1, T, &binary), false)).is_empty());
    }

    /// What `backfill::request` names, `backfill_response` places again: for every shape of
    /// chat and author a request can carry, the phone's answer (which echoes the request's
    /// target) reaches the core as the very message it asked about. A shape the answer could
    /// not be placed for (a chat or an author known by phone number alone, an author nobody
    /// knows) gets no request, so the phone is not asked to upload for nothing.
    #[test]
    fn a_requests_answer_finds_the_message_again_for_every_shape() {
        use proto::sync_message::AttachmentBackfillResponse as Response;
        use proto::sync_message::attachment_backfill_response::{
            AttachmentData, AttachmentDataList, Data,
        };
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let alice = aci_user(peer());
        let pni = crate::service_id::pni_user_id(Pni::from(Uuid::from_u128(0xb0b))).unwrap();
        let number = kourier_model::UserId::parse("e164:+15550100").unwrap();
        let group = crate::groups::conv_key_of_id(&[7; 32]);
        let direct = |user: &kourier_model::UserId| ConvKey::Direct(user.clone());
        let notes = direct(&aci_user(fixture.account.aci));
        let shapes = [
            (direct(&alice), TargetAuthor::User(alice.clone()), true),
            (direct(&alice), TargetAuthor::Own, true),
            (direct(&pni), TargetAuthor::User(pni.clone()), true),
            (notes, TargetAuthor::Own, true),
            (group.clone(), TargetAuthor::User(alice.clone()), true),
            (group.clone(), TargetAuthor::Own, true),
            (direct(&number), TargetAuthor::User(number.clone()), false),
            (direct(&number), TargetAuthor::Own, false),
            (direct(&alice), TargetAuthor::User(number.clone()), false),
            (group, TargetAuthor::User(number), false),
            (direct(&alice), TargetAuthor::Unknown, false),
        ];
        for (n, (conv, author, named)) in shapes.into_iter().enumerate() {
            let target = kourier_model::MsgTarget {
                conv: Some(conv.clone()),
                author: author.clone(),
                local: "1700000000123".into(),
                alternates: Vec::new(),
            };
            let Some(request) = crate::backfill::request(&fixture.account, &conv, &target) else {
                assert!(!named, "shape {n}: a request was expected");
                continue;
            };
            assert!(named, "shape {n}: no request was expected");
            let answer = sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::AttachmentBackfillResponse(
                    Response {
                        target_message: request.target_message,
                        target_conversation: request.target_conversation,
                        data: Some(Data::Attachments(AttachmentDataList {
                            attachments: vec![AttachmentData {
                                data: Some(proto::sync_message::attachment_backfill_response::attachment_data::Data::Status(0)),
                            }],
                            long_text: None,
                        })),
                    },
                )),
                ..Default::default()
            });
            match &events_of(map(&fixture, &row(me, 1, T, &answer), false))[..] {
                [BackendEvent::AttachmentRestore { target: found, .. }] => {
                    assert_eq!(found.conv, Some(conv), "shape {n}");
                    assert_eq!(found.author, author, "shape {n}");
                    assert_eq!(found.local, target.local, "shape {n}");
                }
                other => panic!("shape {n}: {other:?}"),
            }
        }
    }

    /// `SyncMessage.deleteForMe`, `viewed` and `viewOnceOpen` from our phone: message deletes
    /// become deletes for me by our own account (in 1:1 chats, Note to Self and groups);
    /// conversation and attachment deletes have no event and change nothing; views raise the
    /// read state through a `Viewed` receipt of our own.
    #[test]
    fn deletes_for_me_and_views_from_our_phone() {
        use proto::addressable_message::Author;
        use proto::conversation_identifier::Identifier;
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let them: ServiceId = peer().into();
        let thread = |identifier: Identifier| {
            Some(proto::ConversationIdentifier {
                identifier: Some(identifier),
            })
        };
        let addressed = |author: Author, t: u64| proto::AddressableMessage {
            author: Some(author),
            sent_timestamp: Some(t),
        };
        let delete = |delete: proto::sync_message::DeleteForMe| {
            sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::DeleteForMe(delete)),
                ..Default::default()
            })
        };
        let group_id = vec![6u8; 32];
        let messages = delete(proto::sync_message::DeleteForMe {
            message_deletes: vec![
                proto::sync_message::delete_for_me::MessageDeletes {
                    conversation: thread(Identifier::ThreadServiceIdBinary(
                        them.service_id_binary(),
                    )),
                    messages: vec![
                        addressed(Author::AuthorServiceIdBinary(them.service_id_binary()), 10),
                        addressed(Author::AuthorServiceId(me.service_id_string()), 11),
                        // A number alone names nobody here.
                        addressed(Author::AuthorE164("+15550100".to_owned()), 12),
                    ],
                },
                proto::sync_message::delete_for_me::MessageDeletes {
                    conversation: thread(Identifier::ThreadGroupId(group_id.clone())),
                    messages: vec![addressed(
                        Author::AuthorServiceIdBinary(them.service_id_binary()),
                        13,
                    )],
                },
                proto::sync_message::delete_for_me::MessageDeletes {
                    conversation: thread(Identifier::ThreadE164("+15550100".to_owned())),
                    messages: vec![addressed(
                        Author::AuthorServiceIdBinary(them.service_id_binary()),
                        14,
                    )],
                },
            ],
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(me, 1, T, &messages), false));
        let deleted: Vec<(Option<ConvKey>, TargetAuthor, String)> = events
            .iter()
            .map(|event| match event {
                BackendEvent::Deleted {
                    target,
                    by: None,
                    for_everyone: false,
                    ..
                } => (
                    target.conv.clone(),
                    target.author.clone(),
                    target.local.to_string(),
                ),
                other => panic!("{other:?}"),
            })
            .collect();
        let chat = Some(ConvKey::Direct(aci_user(peer())));
        assert_eq!(
            deleted,
            [
                (
                    chat.clone(),
                    TargetAuthor::User(aci_user(peer())),
                    "10".to_owned()
                ),
                (chat, TargetAuthor::Own, "11".to_owned()),
                (
                    Some(crate::groups::conv_key_of_id(&group_id)),
                    TargetAuthor::User(aci_user(peer())),
                    "13".to_owned()
                ),
            ]
        );
        // From anyone but our own other devices it is not a sync message at all.
        assert!(events_of(map(&fixture, &row(them, 1, T, &messages), false)).is_empty());

        // A chat deleted on the phone: its history goes up to the newest messages the phone
        // names (both of its lists are anchors), and with `isFullDelete` the chat itself. A
        // chat deleted where it had nothing to name has no anchors and goes too.
        let uuid = [0xabu8; 16];
        let digest = [0xcdu8; 32];
        let conversation = delete(proto::sync_message::DeleteForMe {
            conversation_deletes: vec![
                proto::sync_message::delete_for_me::ConversationDelete {
                    conversation: thread(Identifier::ThreadServiceIdBinary(
                        them.service_id_binary(),
                    )),
                    most_recent_messages: vec![
                        addressed(Author::AuthorServiceIdBinary(them.service_id_binary()), 30),
                        addressed(Author::AuthorServiceId(me.service_id_string()), 31),
                        addressed(Author::AuthorE164("+15550100".to_owned()), 32),
                    ],
                    most_recent_non_expiring_messages: vec![addressed(
                        Author::AuthorServiceIdBinary(them.service_id_binary()),
                        29,
                    )],
                    is_full_delete: Some(true),
                },
                proto::sync_message::delete_for_me::ConversationDelete {
                    conversation: thread(Identifier::ThreadGroupId(group_id.clone())),
                    most_recent_messages: vec![addressed(
                        Author::AuthorServiceIdBinary(them.service_id_binary()),
                        33,
                    )],
                    ..Default::default()
                },
                // Named by number alone: nothing here can resolve it.
                proto::sync_message::delete_for_me::ConversationDelete {
                    conversation: thread(Identifier::ThreadE164("+15550100".to_owned())),
                    is_full_delete: Some(true),
                    ..Default::default()
                },
            ],
            local_only_conversation_deletes: vec![
                proto::sync_message::delete_for_me::LocalOnlyConversationDelete {
                    conversation: thread(Identifier::ThreadServiceId(me.service_id_string())),
                },
            ],
            attachment_deletes: vec![
                proto::sync_message::delete_for_me::AttachmentDelete {
                    conversation: thread(Identifier::ThreadServiceIdBinary(
                        them.service_id_binary(),
                    )),
                    target_message: Some(addressed(
                        Author::AuthorServiceIdBinary(them.service_id_binary()),
                        40,
                    )),
                    client_uuid: Some(uuid.to_vec()),
                    fallback_digest: Some(digest.to_vec()),
                    fallback_plaintext_hash: Some(vec![9; 32]),
                },
                // Only the plaintext hash: the name of an imported attachment of a file the
                // phone had downloaded.
                proto::sync_message::delete_for_me::AttachmentDelete {
                    conversation: thread(Identifier::ThreadServiceIdBinary(
                        them.service_id_binary(),
                    )),
                    target_message: Some(addressed(
                        Author::AuthorServiceIdBinary(them.service_id_binary()),
                        41,
                    )),
                    fallback_plaintext_hash: Some(vec![9; 32]),
                    ..Default::default()
                },
                // No message named.
                proto::sync_message::delete_for_me::AttachmentDelete {
                    client_uuid: Some(uuid.to_vec()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(me, 1, T + 1, &conversation), false));
        let chat = ConvKey::Direct(aci_user(peer()));
        let at = i64::try_from(T + 1).unwrap();
        let anchor = |conv: &ConvKey, author: TargetAuthor, local: &str| kourier_model::MsgTarget {
            conv: Some(conv.clone()),
            author,
            local: local.to_owned().into(),
            alternates: Vec::new(),
        };
        let group = crate::groups::conv_key_of_id(&group_id);
        let them_user = || TargetAuthor::User(aci_user(peer()));
        assert_eq!(
            events,
            [
                BackendEvent::ConversationCleared {
                    conv: chat.clone(),
                    up_to: vec![
                        anchor(&chat, them_user(), "30"),
                        anchor(&chat, TargetAuthor::Own, "31"),
                        anchor(&chat, them_user(), "29"),
                    ],
                    remove: true,
                    at,
                },
                BackendEvent::ConversationCleared {
                    conv: group.clone(),
                    up_to: vec![anchor(&group, them_user(), "33")],
                    remove: false,
                    at,
                },
                BackendEvent::ConversationCleared {
                    conv: ConvKey::Direct(aci_user(fixture.account.aci)),
                    up_to: Vec::new(),
                    remove: true,
                    at,
                },
                BackendEvent::AttachmentDeleted {
                    target: anchor(&chat, them_user(), "40"),
                    net_ids: kourier_model::Redacted::new(vec![
                        format!("uuid:{}", "ab".repeat(16)),
                        format!("digest:{}", "cd".repeat(32)),
                        format!("plaintext:{}", "09".repeat(32)),
                    ]),
                    at,
                },
                BackendEvent::AttachmentDeleted {
                    target: anchor(&chat, them_user(), "41"),
                    net_ids: kourier_model::Redacted::new(vec![format!(
                        "plaintext:{}",
                        "09".repeat(32)
                    )]),
                    at,
                },
            ]
        );
        // The names are those an incoming attachment of that message carries.
        let pointer = proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "cdn-key".to_owned(),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            digest: Some(digest.to_vec()),
            size: Some(10),
            content_type: Some("image/jpeg".to_owned()),
            client_uuid: Some(uuid.to_vec()),
            ..Default::default()
        };
        let attachment = crate::receive::media::attachment(&pointer, None);
        let BackendEvent::AttachmentDeleted { net_ids, .. } = &events[3] else {
            panic!("{events:?}");
        };
        assert!(
            attachment
                .net_ids
                .iter()
                .all(|name| net_ids.expose().contains(name)),
            "the core finds the attachment by any of its names"
        );
        assert!(
            !format!("{attachment:?}").contains("abab"),
            "names of content stay out of logs"
        );
        // A pointer without a client UUID is still named by its digest.
        let bare = crate::receive::media::attachment(
            &proto::AttachmentPointer {
                client_uuid: None,
                ..pointer
            },
            None,
        );
        assert_eq!(bare.net_ids, [format!("digest:{}", "cd".repeat(32))]);
        // Like every sync message, only from our own other devices.
        assert!(events_of(map(&fixture, &row(them, 1, T + 1, &conversation), false)).is_empty());

        // Viewed on the phone (a voice note), and a view-once message opened there.
        let own = aci_user(fixture.account.aci);
        let viewed = sync(proto::SyncMessage {
            viewed: vec![proto::sync_message::Viewed {
                sender_aci_binary: Some(Uuid::from(peer()).as_bytes().to_vec()),
                timestamp: Some(20),
                ..Default::default()
            }],
            ..Default::default()
        });
        let opened = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::ViewOnceOpen(
                proto::sync_message::ViewOnceOpen {
                    sender_aci: Some(them.service_id_string()),
                    timestamp: Some(21),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        for (content, local) in [(viewed, "20"), (opened, "21")] {
            let events = events_of(map(&fixture, &row(me, 1, T + 2, &content), false));
            let [
                BackendEvent::Receipts {
                    targets,
                    by,
                    kind: ReceiptKind::Viewed,
                    ..
                },
            ] = events.as_slice()
            else {
                panic!("{events:?}");
            };
            assert_eq!(by, &own, "a receipt of our own account");
            assert_eq!(targets.len(), 1);
            assert_eq!(targets[0].author, TargetAuthor::User(aci_user(peer())));
            assert_eq!(&*targets[0].local, local);
        }
    }

    /// Whatever a blocked contact sends is dropped once it is decrypted: messages, edits,
    /// reactions, receipts, typing, retry requests and what did not decrypt (no retry request,
    /// no placeholder). Nothing is owed to them either. Unblocked, they are heard again.
    #[test]
    fn everything_a_blocked_sender_sends_is_dropped() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let sid = sender.service_id_string();
        typing_on(&fixture);
        let block = |blocked: bool| {
            fixture
                .db
                .zone(|tx| {
                    crate::peers::set_profile_sharing(tx, &sid, true, 1)?;
                    crate::peers::set_blocked(tx, &sid, blocked, 1)
                })
                .unwrap();
        };
        let text = data(proto::DataMessage {
            body: Some("hi".to_owned()),
            timestamp: Some(T),
            ..Default::default()
        });
        let reaction = data(proto::DataMessage {
            timestamp: Some(T),
            reaction: Some(proto::data_message::Reaction {
                emoji: Some("👍".to_owned()),
                target_sent_timestamp: Some(5),
                target_author_aci_binary: Some(Uuid::from(fixture.account.aci).as_bytes().to_vec()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let edit = proto::Content {
            content: Some(proto::content::Content::EditMessage(proto::EditMessage {
                target_sent_timestamp: Some(5),
                data_message: Some(proto::DataMessage {
                    body: Some("edited".to_owned()),
                    timestamp: Some(T),
                    ..Default::default()
                }),
            })),
            ..Default::default()
        };
        let receipt = |kind: proto::receipt_message::Type| proto::Content {
            content: Some(proto::content::Content::ReceiptMessage(
                proto::ReceiptMessage {
                    r#type: Some(kind.into()),
                    timestamp: vec![10],
                },
            )),
            ..Default::default()
        };
        let typing = proto::Content {
            content: Some(proto::content::Content::TypingMessage(
                proto::TypingMessage {
                    timestamp: Some(T),
                    action: Some(proto::typing_message::Action::Started.into()),
                    group_id: None,
                },
            )),
            ..Default::default()
        };
        let retry_request = proto::Content {
            content: Some(proto::content::Content::DecryptionErrorMessage(vec![
                1, 2, 3,
            ])),
            ..Default::default()
        };
        let contents = [
            ("text", text.clone()),
            ("reaction", reaction),
            ("edit", edit),
            ("read receipt", receipt(proto::receipt_message::Type::Read)),
            (
                "delivery receipt",
                receipt(proto::receipt_message::Type::Delivery),
            ),
            ("typing", typing),
            ("retry request", retry_request),
        ];
        let sealed = |content: &proto::Content| Row {
            sealed: true,
            ..row(sender, 1, T, content)
        };
        block(true);
        for (what, content) in &contents {
            let outcome = map(&fixture, &sealed(content), false);
            assert_eq!(
                outcome,
                Outcome::Handled {
                    events: Vec::new(),
                    ephemeral: Vec::new(),
                    note: Some("content from a blocked contact"),
                },
                "{what}"
            );
        }
        // A message of theirs that did not decrypt: no placeholder, no retry request.
        let undecryptable = Row {
            content: None,
            decrypt_error: Some(vec![1, 2, 3]),
            ..row(sender, 1, T, &text)
        };
        assert!(events_of(map(&fixture, &undecryptable, false)).is_empty());
        assert_eq!(count(&fixture.db, "delivery_receipts"), 0, "nothing owed");
        assert_eq!(
            count(&fixture.db, "jobs"),
            0,
            "no receipt, no retry request"
        );

        // Unblocked on the phone: the same text arrives and is owed its receipt.
        block(false);
        let events = events_of(map(&fixture, &sealed(&text), false));
        assert!(
            matches!(events.as_slice(), [BackendEvent::Message(_)]),
            "{events:?}"
        );
        assert_eq!(count(&fixture.db, "delivery_receipts"), 1);
    }

    /// A message with more attachments than the official clients keep (they cut at 32 for
    /// their own display) is shown whole: every attachment, in order.
    #[test]
    fn a_message_with_more_attachments_than_desktop_keeps_is_shown_whole() {
        let fixture = fixture();
        let pointer = |n: u8| proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                format!("cdn-key-{n}"),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            digest: Some(vec![n; 32]),
            size: Some(4096),
            content_type: Some("image/jpeg".to_owned()),
            ..Default::default()
        };
        let album = data(proto::DataMessage {
            body: Some("forty".to_owned()),
            timestamp: Some(T),
            attachments: (0..40).map(pointer).collect(),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(peer().into(), 1, T, &album), false));
        let [BackendEvent::Message(m)] = events.as_slice() else {
            panic!("{events:?}");
        };
        let Body::Media { attachments, .. } = &m.body else {
            panic!("{:?}", m.body);
        };
        assert_eq!(attachments.len(), 40);
        let digests: Vec<String> = attachments.iter().map(|a| a.net_ids.join(",")).collect();
        let expected: Vec<String> = (0..40u8)
            .map(|n| format!("digest:{}", format!("{n:02x}").repeat(32)))
            .collect();
        assert_eq!(
            digests, expected,
            "in the sender's order, each with its name"
        );
    }

    /// No client applies an edit to a view-once message, a contact share or a voice message;
    /// an edit states its message whole, so one that is itself any of these is dropped.
    #[test]
    fn edits_of_view_once_contact_and_voice_messages_are_dropped() {
        let fixture = fixture();
        let voice = proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "cdn-key".to_owned(),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            digest: Some(vec![2; 32]),
            size: Some(4096),
            content_type: Some("audio/aac".to_owned()),
            flags: Some(1),
            ..Default::default()
        };
        let edit = |message: proto::DataMessage| {
            let content = proto::Content {
                content: Some(proto::content::Content::EditMessage(proto::EditMessage {
                    target_sent_timestamp: Some(39),
                    data_message: Some(proto::DataMessage {
                        body: Some("a caption for it".to_owned()),
                        timestamp: Some(T + 1),
                        ..message
                    }),
                })),
                ..Default::default()
            };
            map(&fixture, &row(peer().into(), 1, T + 1, &content), false)
        };
        let dropped = |outcome: Outcome, why: &'static str| {
            assert_eq!(
                outcome,
                Outcome::Handled {
                    events: Vec::new(),
                    ephemeral: Vec::new(),
                    note: Some(why),
                }
            );
        };
        dropped(
            edit(proto::DataMessage {
                is_view_once: Some(true),
                ..Default::default()
            }),
            "edit of a view-once message",
        );
        dropped(
            edit(proto::DataMessage {
                contact: vec![proto::data_message::Contact::default()],
                ..Default::default()
            }),
            "edit of a contact share",
        );
        dropped(
            edit(proto::DataMessage {
                attachments: vec![voice.clone()],
                ..Default::default()
            }),
            "edit of a voice message",
        );
        // An audio file that is no voice note takes its caption.
        let events = events_of(edit(proto::DataMessage {
            attachments: vec![proto::AttachmentPointer {
                flags: None,
                ..voice
            }],
            ..Default::default()
        }));
        assert!(
            matches!(events.as_slice(), [BackendEvent::Edited { .. }]),
            "{events:?}"
        );
    }

    /// A group admin's delete of another member's message names that member as the target's
    /// author and the admin as the deleter; the core checks the role and the window.
    #[test]
    fn an_admin_delete_in_a_group_targets_the_authors_message() {
        use super::super::dispatch::{Who, content_events};
        let fixture = fixture();
        let admin = aci_user(peer());
        let author = Aci::from(Uuid::from_u128(0xb0b));
        let conv = crate::groups::conv_key_of_id(&[6u8; 32]);
        let message = proto::DataMessage {
            timestamp: Some(T),
            admin_delete: Some(proto::data_message::AdminDelete {
                target_author_aci_binary: Some(Uuid::from(author).as_bytes().to_vec()),
                target_sent_timestamp: Some(77),
            }),
            ..Default::default()
        };
        let content = data(message.clone());
        let row = row(peer().into(), 1, T, &content);
        let ctx = Ctx {
            account: &fixture.account,
            catch_up: false,
            now: NOW,
            long_text: None,
        };
        let who = Who::Remote {
            user: admin.clone(),
            device: 1,
        };
        let events = events_of(content_events(
            &ctx,
            &row,
            &who,
            conv.clone(),
            &message,
            T,
            9,
        ));
        let [
            BackendEvent::Deleted {
                target,
                by,
                for_everyone: true,
                at: 9,
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(by.as_ref(), Some(&admin));
        assert_eq!(target.author, TargetAuthor::User(aci_user(author)));
        assert_eq!(&*target.local, "77");
        assert_eq!(target.conv.as_ref(), Some(&conv));
    }

    #[test]
    fn invalid_content_is_a_content_failure() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let failed = |content: proto::Content, timestamp: u64| {
            matches!(
                map(&fixture, &row(sender, 1, timestamp, &content), false),
                Outcome::Failed { visible: true, .. }
            )
        };
        assert!(failed(text_content("x", T), T + 1), "timestamp differs");
        let mut flagged = text_content("x", T);
        if let Some(proto::content::Content::DataMessage(m)) = &mut flagged.content {
            m.flags = Some(8);
        }
        assert!(failed(flagged, T), "unknown flag");
        let mut garbage = row(sender, 1, T, &text_content("x", T));
        garbage.content = Some(vec![0x0a, 0xff]);
        assert!(matches!(
            map(&fixture, &garbage, false),
            Outcome::Failed {
                reason: "undecodable content",
                visible: true
            }
        ));
    }

    #[test]
    fn receipts_and_typing() {
        let fixture = fixture();
        let sender: ServiceId = peer().into();
        let receipt = proto::Content {
            content: Some(proto::content::Content::ReceiptMessage(
                proto::ReceiptMessage {
                    r#type: Some(proto::receipt_message::Type::Read.into()),
                    timestamp: vec![10, 11],
                },
            )),
            ..Default::default()
        };
        let events = events_of(map(&fixture, &row(sender, 1, T, &receipt), false));
        let [
            BackendEvent::Receipts {
                targets,
                by,
                kind: ReceiptKind::Read,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(*by, aci_user(peer()));
        let locals: Vec<&str> = targets.iter().map(|t| &*t.local).collect();
        assert_eq!(locals, ["10", "11"]);
        assert!(
            targets
                .iter()
                .all(|t| t.author == TargetAuthor::Own && t.conv.is_none())
        );

        let typing = |action: proto::typing_message::Action| proto::Content {
            content: Some(proto::content::Content::TypingMessage(
                proto::TypingMessage {
                    timestamp: Some(T),
                    action: Some(action.into()),
                    group_id: None,
                },
            )),
            ..Default::default()
        };
        let started = row(
            sender,
            4,
            T,
            &typing(proto::typing_message::Action::Started),
        );
        let Outcome::Handled { ephemeral, .. } = map(&fixture, &started, false) else {
            panic!("handled");
        };
        assert!(ephemeral.is_empty(), "the typing setting is off by default");
        typing_on(&fixture);
        let Outcome::Handled {
            events, ephemeral, ..
        } = map(&fixture, &started, false)
        else {
            panic!("handled");
        };
        assert!(events.is_empty(), "typing is never durable");
        assert_eq!(
            ephemeral,
            [BackendEvent::Typing {
                conv: ConvKey::Direct(aci_user(peer())),
                by: aci_user(peer()),
                device: Some(4),
                on: true,
                ttl: crate::receive::dispatch::TYPING_TTL,
            }]
        );
        let Outcome::Handled { ephemeral, .. } = map(&fixture, &started, true) else {
            panic!("handled");
        };
        assert!(ephemeral.is_empty(), "stale while catching up");
        let stopped = row(
            sender,
            4,
            T,
            &typing(proto::typing_message::Action::Stopped),
        );
        let Outcome::Handled { ephemeral, .. } = map(&fixture, &stopped, false) else {
            panic!("handled");
        };
        assert!(matches!(
            ephemeral[..],
            [BackendEvent::Typing { on: false, .. }]
        ));
    }

    #[test]
    fn sync_messages_only_from_our_other_devices() {
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let transcript = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Sent(
                proto::sync_message::Sent {
                    destination_service_id_binary: Some(
                        ServiceId::from(peer()).service_id_binary(),
                    ),
                    timestamp: Some(T),
                    message: Some(proto::DataMessage {
                        body: Some("from my phone".to_owned()),
                        timestamp: Some(T),
                        expire_timer: Some(30),
                        ..Default::default()
                    }),
                    expiration_start_timestamp: Some(T + 3),
                    unidentified_status: vec![
                        proto::sync_message::sent::UnidentifiedDeliveryStatus {
                            destination_service_id_binary: Some(
                                ServiceId::from(peer()).service_id_binary(),
                            ),
                            unidentified: Some(true),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(me, 1, T, &transcript), false));
        let [
            BackendEvent::Message(m),
            BackendEvent::OwnSendState {
                target,
                per_recipient,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(m.origin, Origin::OwnOtherDevice { device: Some(1) });
        assert_eq!(m.conv, ConvKey::Direct(aci_user(peer())));
        assert_eq!(m.body, Body::text("from my phone"));
        assert_eq!(
            m.expiry,
            Some(Expiry {
                kind: DisappearKind::AfterSend,
                timer: Duration::from_secs(30),
                started_at: Some(i64::try_from(T + 3).unwrap()),
            })
        );
        assert_eq!(target.author, TargetAuthor::Own);
        assert_eq!(
            *per_recipient,
            [(aci_user(peer()), RecipientState::Sent { sealed: true })]
        );
        assert_eq!(
            crate::receive::stage1::lane_of(
                &crate::receive::stage1::Sender {
                    service_id: me,
                    device: DeviceId::new(1).unwrap(),
                },
                Some(&transcript),
            ),
            ServiceId::from(peer()).service_id_string(),
            "a transcript runs in the peer's lane"
        );

        // From this very device, or from another account: dropped.
        let ours = u32::from(fixture.account.device_id);
        assert!(events_of(map(&fixture, &row(me, ours, T, &transcript), false)).is_empty());
        assert!(events_of(map(&fixture, &row(peer().into(), 1, T, &transcript), false)).is_empty());

        let read = sync(proto::SyncMessage {
            read: vec![proto::sync_message::Read {
                sender_aci_binary: Some(Uuid::from(peer()).as_bytes().to_vec()),
                timestamp: Some(77),
                ..Default::default()
            }],
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(me, 1, T, &read), false));
        let [BackendEvent::ReadMarker { conv: None, up_to }] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert_eq!(up_to.author, TargetAuthor::User(aci_user(peer())));
        assert_eq!(&*up_to.local, "77");
        assert_eq!(
            crate::receive::stage1::lane_of(
                &crate::receive::stage1::Sender {
                    service_id: me,
                    device: DeviceId::new(1).unwrap(),
                },
                Some(&read),
            ),
            MISC_LANE
        );

        let configuration = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Configuration(
                proto::sync_message::Configuration {
                    read_receipts: Some(true),
                    typing_indicators: Some(false),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(me, 1, T, &configuration), false));
        let [BackendEvent::AccountSettings(settings)] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert_eq!(settings.read_receipts, Some(true));
        assert_eq!(settings.typing_indicators, Some(false));

        let block = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::MessageRequestResponse(
                proto::sync_message::MessageRequestResponse {
                    thread_aci: Some(PEER.to_owned()),
                    r#type: Some(proto::sync_message::message_request_response::Type::Block.into()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(me, 1, T, &block), false));
        let [
            BackendEvent::ContactUpsert(contact),
            BackendEvent::ConversationUpsert(conv),
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(contact.blocked, Some(true));
        assert_eq!(
            (conv.blocked, conv.request_pending),
            (Some(true), Some(false))
        );
    }

    /// The raw outcome of `dispatch`, features included.
    fn map_raw(fixture: &Fixture, row: &Row) -> Vec<BackendEvent> {
        let outcome = fixture
            .db
            .zone(|tx| {
                dispatch(
                    tx,
                    &Ctx {
                        account: &fixture.account,
                        catch_up: false,
                        now: 1,
                        long_text: None,
                    },
                    row,
                )
            })
            .unwrap();
        events_of(outcome)
    }

    fn peer_sid() -> String {
        ServiceId::from(peer()).service_id_string()
    }

    fn owed_receipts(fixture: &Fixture) -> i64 {
        count(&fixture.db, "delivery_receipts")
    }

    fn accept(fixture: &Fixture) {
        fixture
            .db
            .zone(|tx| crate::peers::set_profile_sharing(tx, &peer_sid(), true, 1).map(|_| ()))
            .unwrap();
    }

    fn call(message: proto::CallMessage) -> proto::Content {
        proto::Content {
            content: Some(proto::content::Content::CallMessage(message)),
            ..Default::default()
        }
    }

    fn offer(id: u64, video: bool) -> proto::CallMessage {
        use proto::call_message::offer::Type;
        proto::CallMessage {
            offer: Some(proto::call_message::Offer {
                id: Some(id),
                r#type: Some(
                    if video {
                        Type::OfferVideoCall
                    } else {
                        Type::OfferAudioCall
                    }
                    .into(),
                ),
                opaque: None,
            }),
            ..Default::default()
        }
    }

    fn call_line(event: &BackendEvent) -> (&kourier_backend_api::IncomingMessage, &CallLine) {
        let BackendEvent::Message(message) = event else {
            panic!("not a message: {event:?}");
        };
        let line = message.body.call_line().expect("a call line");
        (message, line)
    }

    /// A group with `members` (and us), known to the core.
    fn store_group(fixture: &Fixture, master: [u8; 32], members: &[Aci]) -> GroupKeys {
        let keys = GroupKeys::from_slice(&master).unwrap();
        let snapshot = Snapshot {
            revision: 1,
            members: members
                .iter()
                .chain([&fixture.account.aci])
                .map(|aci| SnapMember {
                    aci: (*aci).into(),
                    admin: false,
                    joined_at: 0,
                })
                .collect(),
            ..Snapshot::default()
        };
        fixture
            .db
            .zone(|tx| -> crate::error::Result<()> {
                tx.execute(
                    "INSERT INTO groups(master_key, group_id, updated_at, revision, state,
                       membership, features_hash) VALUES (?1, ?2, 0, 1, ?3, 'member', 42)",
                    (&keys.master[..], &keys.id[..], snapshot.to_json()?),
                )?;
                Ok(())
            })
            .unwrap();
        keys
    }

    /// The Offer of an accepted contact rings for what is left of a minute by the server's
    /// clock, the same on every replay of the row; a stranger's never rings and is silent;
    /// a call message for another of our devices, or from our own account, is nothing. And
    /// no row of a call queues anything to send.
    #[test]
    fn a_call_message_becomes_a_call_line_and_queues_nothing() {
        let fixture = fixture();
        let sender = ServiceId::from(peer());
        let caller = aci_user(peer());
        let taken = i64::try_from(T).unwrap() + 5;
        let waited = |ms: i64, content: &proto::Content| Row {
            sealed: true,
            delivered_at: Some(taken + ms),
            ..row(sender, 3, T, content)
        };

        // Not accepted yet: no time left, never announced.
        let events = events_of(map(&fixture, &waited(0, &call(offer(41, false))), false));
        let [event] = events.as_slice() else {
            panic!("{events:?}");
        };
        let (message, line) = call_line(event);
        assert_eq!(line.deadline(), Some(message.sent_at), "no ringing phase");
        assert!(message.silent);

        accept(&fixture);
        let fresh = waited(250, &call(offer(42, true)));
        let events = events_of(map(&fixture, &fresh, true));
        let [event] = events.as_slice() else {
            panic!("{events:?}");
        };
        let (message, line) = call_line(event);
        assert_eq!(message.conv, ConvKey::Direct(caller.clone()));
        assert_eq!(&*message.local, "call:42");
        assert_eq!(
            message.origin,
            Origin::Remote {
                author: caller.clone(),
                device: Some(3)
            }
        );
        assert_eq!(
            message.sent_at, taken,
            "the server's clock when it took the envelope: the caller cannot choose it"
        );
        assert_ne!(message.sent_at, i64::try_from(T).unwrap());
        assert_eq!(message.received_at, 100, "the row's, not the clock's");
        assert_eq!(message.server_at, Some(taken));
        assert_eq!(message.arrival, Some(7));
        assert!(message.catch_up && !message.silent);
        assert_eq!(
            line,
            &CallLine::signal(
                CallDirection::Incoming,
                false,
                CallState::Ringing {
                    deadline: message.sent_at + 60_000 - 250
                },
                message.sent_at
            )
            .with_media(CallMedia::Video, message.sent_at)
            .with_by(caller.clone(), message.sent_at)
        );
        assert!(line.may_come_from_peer());
        // A replay of the row is the same signal, to the millisecond.
        assert_eq!(events_of(map(&fixture, &fresh, true)), events);

        // 59 s and 61 s on the server; no delivery time at all counts as no age.
        for (ms, left) in [(59_000, 1_000), (61_000, -1_000)] {
            let events = events_of(map(&fixture, &waited(ms, &call(offer(43, false))), false));
            let (message, line) = call_line(&events[0]);
            assert_eq!(line.deadline(), Some(message.sent_at + left));
        }
        let unknown = Row {
            delivered_at: None,
            ..waited(0, &call(offer(44, false)))
        };
        let events = events_of(map(&fixture, &unknown, false));
        let (message, line) = call_line(&events[0]);
        assert_eq!(line.deadline(), Some(message.sent_at + 60_000));

        // The caller gives up: missed. Any other hang-up: nothing.
        use proto::call_message::hangup::Type as Hangup;
        let hangup = |kind: Option<Hangup>| {
            call(proto::CallMessage {
                hangup: Some(proto::call_message::Hangup {
                    id: Some(42),
                    r#type: kind.map(Into::into),
                    device_id: Some(1),
                }),
                ..Default::default()
            })
        };
        for kind in [Some(Hangup::HangupNormal), None] {
            let events = events_of(map(&fixture, &waited(0, &hangup(kind)), false));
            let [event] = events.as_slice() else {
                panic!("{kind:?}: {events:?}");
            };
            let (message, line) = call_line(event);
            assert_eq!(&*message.local, "call:42");
            assert_eq!(line.state, CallState::Missed);
            assert_eq!(line.by.as_ref(), Some(&caller));
            assert_eq!(line.media, None);
            assert!(line.may_come_from_peer());
        }
        for kind in [
            Hangup::HangupAccepted,
            Hangup::HangupDeclined,
            Hangup::HangupBusy,
            Hangup::HangupNeedPermission,
        ] {
            let outcome = map(&fixture, &waited(0, &hangup(Some(kind))), false);
            assert!(events_of(outcome).is_empty(), "{kind:?}");
        }

        // For another of our devices, from our own account, and the signalling of a call
        // that is taken elsewhere: nothing.
        let our_device = u32::from(fixture.account.device_id);
        let elsewhere = call(proto::CallMessage {
            destination_device_id: Some(our_device + 1),
            ..offer(45, false)
        });
        assert!(events_of(map(&fixture, &waited(0, &elsewhere), false)).is_empty());
        let here = call(proto::CallMessage {
            destination_device_id: Some(our_device),
            ..offer(45, false)
        });
        assert_eq!(events_of(map(&fixture, &waited(0, &here), false)).len(), 1);
        let own = Row {
            sender: ServiceId::from(fixture.account.aci).service_id_string(),
            ..waited(0, &call(offer(46, false)))
        };
        assert!(events_of(map(&fixture, &own, false)).is_empty());
        let rest = call(proto::CallMessage {
            answer: Some(proto::call_message::Answer {
                id: Some(42),
                opaque: None,
            }),
            busy: Some(proto::call_message::Busy { id: Some(42) }),
            ice_update: vec![proto::call_message::IceUpdate {
                id: Some(42),
                opaque: None,
            }],
            ..Default::default()
        });
        assert!(events_of(map(&fixture, &waited(0, &rest), false)).is_empty());

        // Sealed rows of an accepted contact, and still nothing is owed or queued.
        assert_eq!(owed_receipts(&fixture), 0);
        assert_eq!(count(&fixture.db, "jobs"), 0);
    }

    /// A ring is read before an Offer beside it and is a line of the group; a group-call
    /// update is the line its era names, owes no delivery receipt although it came sealed,
    /// and in our own transcript says "answered" and names nobody; the phone's sync finds
    /// the same line by the ring id.
    #[test]
    fn group_call_signals_meet_in_one_line_and_queue_nothing() {
        use crate::receive::calls::{call_id_text, ring_id_of_era};
        let fixture = fixture();
        let sender = ServiceId::from(peer());
        let ringer = aci_user(peer());
        accept(&fixture);
        let keys = store_group(&fixture, [9; 32], &[peer()]);
        let conv = keys.conv_key();
        let ring = |group_id: &[u8], ring_id: i64, cancelled: bool| {
            use crate::proto::ringrtc;
            proto::call_message::Opaque {
                data: Some(
                    ringrtc::CallMessage {
                        group_call_message: None,
                        ring_intention: Some(ringrtc::call_message::RingIntention {
                            group_id: Some(group_id.to_vec()),
                            r#type: Some(i32::from(cancelled)),
                            ring_id: Some(ring_id),
                        }),
                    }
                    .encode_to_vec(),
                ),
                urgency: None,
            }
        };
        let sealed = |content: &proto::Content| Row {
            sealed: true,
            ..row(sender, 1, T, content)
        };

        // A ring with an Offer beside it: one group line, no 1:1 line.
        let both = call(proto::CallMessage {
            opaque: Some(ring(&keys.id, -7, false)),
            ..offer(77, true)
        });
        let events = events_of(map(&fixture, &sealed(&both), false));
        let [event] = events.as_slice() else {
            panic!("{events:?}");
        };
        let (message, line) = call_line(event);
        assert_eq!(message.conv, conv);
        assert_eq!(&*message.local, "gcall:18446744073709551609");
        assert!(line.group && line.media.is_none());
        assert_eq!(line.deadline(), Some(message.sent_at + 60_000));
        assert_eq!(line.by.as_ref(), Some(&ringer));
        assert!(line.may_come_from_peer());

        let cancelled = call(proto::CallMessage {
            opaque: Some(ring(&keys.id, -7, true)),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &sealed(&cancelled), false));
        let (message, line) = call_line(&events[0]);
        assert_eq!(&*message.local, "gcall:18446744073709551609");
        assert_eq!(line.state, CallState::Missed);

        // An unknown group, a ringer who is no member, and an opaque payload without a ring
        // beside an Offer (the Offer is then a 1:1 call).
        let unknown = call(proto::CallMessage {
            opaque: Some(ring(&[4; 32], 1, false)),
            ..Default::default()
        });
        assert!(events_of(map(&fixture, &sealed(&unknown), false)).is_empty());
        let outsider = Row {
            sender: ServiceId::from(Aci::from(Uuid::from_u128(0xd00d))).service_id_string(),
            ..sealed(&both)
        };
        assert!(events_of(map(&fixture, &outsider, false)).is_empty());
        let no_ring = call(proto::CallMessage {
            opaque: Some(proto::call_message::Opaque {
                data: Some(b"\xff\xff\xff".to_vec()),
                urgency: None,
            }),
            ..offer(78, false)
        });
        let events = events_of(map(&fixture, &sealed(&no_ring), false));
        assert_eq!(&*call_line(&events[0]).0.local, "call:78");
        let nothing = call(proto::CallMessage {
            opaque: Some(proto::call_message::Opaque::default()),
            ..Default::default()
        });
        assert!(events_of(map(&fixture, &sealed(&nothing), false)).is_empty());

        // A member's group-call update, sealed: the line of its era, started, by the member.
        let era = "00000000000000ff";
        let update = |era: Option<&str>| proto::DataMessage {
            timestamp: Some(T),
            group_v2: Some(proto::GroupContextV2 {
                master_key: Some(keys.master.to_vec()),
                revision: Some(1),
                ..Default::default()
            }),
            group_call_update: Some(proto::data_message::GroupCallUpdate {
                era_id: era.map(str::to_owned),
            }),
            ..Default::default()
        };
        let events = events_of(map(&fixture, &sealed(&data(update(Some(era)))), false));
        let [event] = events.as_slice() else {
            panic!("{events:?}");
        };
        let (message, line) = call_line(event);
        assert_eq!(message.conv, conv);
        assert_eq!(&*message.local, "gcall:255");
        assert_eq!(call_id_text(ring_id_of_era(era)), "255");
        assert_eq!(line.state, CallState::Started);
        assert_eq!(line.by.as_ref(), Some(&ringer));
        assert!(line.group && line.may_come_from_peer());
        assert!(matches!(message.origin, Origin::Remote { .. }));
        assert!(events_of(map(&fixture, &sealed(&data(update(None))), false)).is_empty());
        assert_eq!(owed_receipts(&fixture), 0, "no receipt for a call");

        // The same update in our phone's transcript: answered, by nobody, no send state.
        let own = ServiceId::from(fixture.account.aci);
        let transcript = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Sent(
                proto::sync_message::Sent {
                    timestamp: Some(T),
                    message: Some(update(Some(era))),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        let events = events_of(map(&fixture, &row(own, 1, T, &transcript), false));
        let [event] = events.as_slice() else {
            panic!("{events:?}");
        };
        let (message, line) = call_line(event);
        assert_eq!(&*message.local, "gcall:255");
        assert_eq!(message.origin, Origin::OwnOtherDevice { device: Some(1) });
        assert_eq!(line.state, CallState::Answered);
        assert_eq!(line.direction, CallDirection::Incoming);
        assert!(line.by.is_none() && line.group);

        // The phone's sync names the ring id as an unsigned call id, and its delete the line.
        use proto::sync_message::call_event::{Direction, Event, Type};
        let synced = |event: Event| {
            sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::CallEvent(
                    proto::sync_message::CallEvent {
                        conversation_id: Some(keys.id.to_vec()),
                        call_id: Some((-7i64).cast_unsigned()),
                        timestamp: Some(T + 9),
                        r#type: Some(Type::GroupCall.into()),
                        direction: Some(Direction::Incoming.into()),
                        event: Some(event.into()),
                    },
                )),
                ..Default::default()
            })
        };
        let events = events_of(map(
            &fixture,
            &row(own, 1, T, &synced(Event::Accepted)),
            false,
        ));
        let (message, line) = call_line(&events[0]);
        assert_eq!(message.conv, conv);
        assert_eq!(&*message.local, "gcall:18446744073709551609");
        assert_eq!(message.sent_at, i64::try_from(T + 9).unwrap());
        assert_eq!(line.state, CallState::Answered);
        assert!(line.by.is_none() && line.media.is_none() && line.group);
        let events = events_of(map(
            &fixture,
            &row(own, 1, T, &synced(Event::Delete)),
            false,
        ));
        let [
            BackendEvent::Deleted {
                target,
                by: None,
                for_everyone: false,
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert_eq!(target.conv.as_ref(), Some(&conv));
        assert_eq!(&*target.local, "gcall:18446744073709551609");
        assert_eq!(target.author, TargetAuthor::Own);
        // The same sync from somebody else's account is no sync at all.
        assert!(
            events_of(map(
                &fixture,
                &row(sender, 1, T, &synced(Event::Accepted)),
                false
            ))
            .is_empty()
        );
        assert_eq!(count(&fixture.db, "jobs"), 0);
        assert_eq!(owed_receipts(&fixture), 0);
    }

    #[test]
    fn sealed_messages_in_accepted_chats_owe_a_delivery_receipt() {
        let fixture = fixture();
        let sender = ServiceId::from(peer());
        let text = |t: u64| {
            data(proto::DataMessage {
                body: Some("hi".to_owned()),
                timestamp: Some(t),
                ..Default::default()
            })
        };
        let sealed = |t: u64| Row {
            sealed: true,
            ..row(sender, 1, t, &text(t))
        };
        map(&fixture, &sealed(T), false);
        assert_eq!(owed_receipts(&fixture), 0, "not accepted: no receipt");
        accept(&fixture);
        map(&fixture, &row(sender, 1, T + 1, &text(T + 1)), false);
        assert_eq!(owed_receipts(&fixture), 0, "unsealed: the server's receipt");
        map(&fixture, &sealed(T + 2), false);
        assert_eq!(owed_receipts(&fixture), 1);
        let job = fixture
            .db
            .read(|c| crate::jobs::next_due(c, i64::MAX, true, false))
            .unwrap()
            .unwrap();
        assert_eq!(job.kind, crate::jobs::JobKind::DeliveryReceipts);
        // A reaction is no timeline message: no receipt (Desktop's reaction path).
        let reaction = data(proto::DataMessage {
            timestamp: Some(T + 3),
            reaction: Some(proto::data_message::Reaction {
                emoji: Some("👍".to_owned()),
                target_sent_timestamp: Some(T),
                target_author_aci_binary: Some(Uuid::from(fixture.account.aci).as_bytes().to_vec()),
                ..Default::default()
            }),
            ..Default::default()
        });
        map(
            &fixture,
            &Row {
                sealed: true,
                ..row(sender, 1, T + 3, &reaction)
            },
            false,
        );
        assert_eq!(owed_receipts(&fixture), 1);
    }

    #[test]
    fn delivery_receipts_settle_the_send_log_and_the_pni_signature() {
        let fixture = fixture();
        let sender = ServiceId::from(peer());
        fixture
            .db
            .zone(|tx| -> crate::error::Result<()> {
                tx.execute(
                    "INSERT INTO peers(service_id, share_my_number, updated_at) VALUES (?1, 1, 0)",
                    [peer_sid()],
                )?;
                crate::msl::record(
                    tx,
                    &crate::msl::Payload {
                        timestamp: 77,
                        content: b"c",
                        hint: libsignal_protocol::ContentHint::Resendable,
                        urgent: true,
                        has_pni_signature: true,
                        local: Some("77"),
                        group_id: None,
                    },
                    &peer_sid(),
                    &[DeviceId::new(1).unwrap()],
                    0,
                )?;
                Ok(())
            })
            .unwrap();
        let receipt = proto::Content {
            content: Some(proto::content::Content::ReceiptMessage(
                proto::ReceiptMessage {
                    r#type: Some(proto::receipt_message::Type::Delivery.into()),
                    timestamp: vec![77],
                },
            )),
            ..Default::default()
        };
        let events = events_of(map(&fixture, &row(sender, 1, T, &receipt), false));
        assert!(matches!(
            events.as_slice(),
            [BackendEvent::Receipts {
                kind: ReceiptKind::Delivered,
                ..
            }]
        ));
        assert_eq!(count(&fixture.db, "msl_payloads"), 0);
        let peer = fixture
            .db
            .read(|c| crate::peers::load(c, &peer_sid()))
            .unwrap();
        assert!(!peer.share_my_number, "they merged our number");
    }

    #[test]
    fn the_phone_accepts_chats_by_writing_in_them_or_accepting_the_request() {
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let features = |events: &[BackendEvent]| -> Vec<Option<kourier_model::CannotSend>> {
            events
                .iter()
                .filter_map(|e| match e {
                    BackendEvent::ConversationUpsert(p) => {
                        p.features.as_ref().map(|f| f.cannot_send.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        // A message from the peer: the chat is not accepted yet.
        let hello = data(proto::DataMessage {
            body: Some("hello".to_owned()),
            timestamp: Some(T),
            ..Default::default()
        });
        let events = map_raw(&fixture, &row(ServiceId::from(peer()), 1, T, &hello));
        assert_eq!(
            features(&events),
            [Some(kourier_model::CannotSend::NotAccepted)]
        );
        // Our phone writes to them with our profile key: accepted.
        let transcript = |timestamp: u64, profile_key: Option<Vec<u8>>, flags: Option<u32>| {
            sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::Sent(
                    proto::sync_message::Sent {
                        destination_service_id_binary: Some(
                            ServiceId::from(peer()).service_id_binary(),
                        ),
                        timestamp: Some(timestamp),
                        message: Some(proto::DataMessage {
                            body: flags.is_none().then(|| "from the phone".to_owned()),
                            timestamp: Some(timestamp),
                            profile_key,
                            flags,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })
        };
        let events = map_raw(&fixture, &row(me, 1, T + 1, &transcript(T + 1, None, None)));
        assert!(
            features(&events).is_empty(),
            "no profile key: nothing changes"
        );
        let events = map_raw(
            &fixture,
            &row(me, 1, T + 2, &transcript(T + 2, Some(vec![1; 32]), None)),
        );
        assert_eq!(features(&events), [None], "accepted");
        assert!(events.iter().any(|e| matches!(e, BackendEvent::Message(_))));

        // Blocking on the phone turns sharing off, accepting on again.
        let response = |kind: proto::sync_message::message_request_response::Type| {
            sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::MessageRequestResponse(
                    proto::sync_message::MessageRequestResponse {
                        thread_aci_binary: Some(Uuid::from(peer()).as_bytes().to_vec()),
                        r#type: Some(kind.into()),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })
        };
        use proto::sync_message::message_request_response::Type;
        let events = map_raw(&fixture, &row(me, 1, T + 3, &response(Type::Block)));
        assert_eq!(
            features(&events),
            [Some(kourier_model::CannotSend::NotAccepted)]
        );
        let events = map_raw(&fixture, &row(me, 1, T + 4, &response(Type::Accept)));
        assert_eq!(features(&events), [None]);

        // A profile-key update transcript is no message but accepts the chat too.
        map_raw(&fixture, &row(me, 1, T + 4, &response(Type::Block)));
        let events = map_raw(
            &fixture,
            &row(me, 1, T + 5, &transcript(T + 5, Some(vec![1; 32]), Some(4))),
        );
        assert_eq!(features(&events), [None]);
        assert!(!events.iter().any(|e| matches!(e, BackendEvent::Message(_))));
    }

    #[test]
    fn configuration_syncs_are_stored_for_the_send_path() {
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let configuration = sync(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Configuration(
                proto::sync_message::Configuration {
                    read_receipts: Some(true),
                    typing_indicators: Some(true),
                    ..Default::default()
                },
            )),
            ..Default::default()
        });
        map(&fixture, &row(me, 1, T, &configuration), false);
        let settings = fixture.db.read(crate::db::synced_settings).unwrap();
        assert!(settings.read_receipts && settings.typing_indicators);
        assert!(!settings.share_number_with_everybody);
    }

    #[test]
    fn timer_changes_and_session_resets_from_our_phone_are_own_notices() {
        let fixture = fixture();
        let me: ServiceId = fixture.account.aci.into();
        let flags_transcript = |timestamp: u64, flags: u32| {
            sync(proto::SyncMessage {
                content: Some(proto::sync_message::Content::Sent(
                    proto::sync_message::Sent {
                        destination_service_id_binary: Some(
                            ServiceId::from(peer()).service_id_binary(),
                        ),
                        timestamp: Some(timestamp),
                        message: Some(proto::DataMessage {
                            timestamp: Some(timestamp),
                            flags: Some(flags),
                            expire_timer: Some(60),
                            expire_timer_version: Some(3),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })
        };
        let events = events_of(map(
            &fixture,
            &row(me, 1, T, &flags_transcript(T, 2)),
            false,
        ));
        let message = messages(&events)[0];
        assert_eq!(message.origin, Origin::OwnOtherDevice { device: Some(1) });
        assert_eq!(
            message.body,
            Body::System(SystemEvent::TimerChanged {
                by: None,
                timer: Some(Duration::from_secs(60)),
            })
        );
        let version = fixture
            .db
            .read(|c| crate::peers::load(c, &peer_sid()))
            .unwrap()
            .expire_timer_version;
        assert_eq!(version, 3, "our own timer version counts too");
        let events = events_of(map(
            &fixture,
            &row(me, 1, T + 1, &flags_transcript(T + 1, 1)),
            false,
        ));
        assert!(matches!(
            messages(&events)[0].body,
            Body::System(SystemEvent::SessionRefreshed { .. })
        ));
    }
}

// -- poison rules and lanes ----------------------------------------------------------------------

/// Inserts an inbox row by hand.
fn insert_row(
    fixture: &Fixture,
    arrival: i64,
    lane: &str,
    sender: &str,
    content: &[u8],
    received_at: i64,
) {
    fixture
        .db
        .zone(|tx| -> crate::error::Result<()> {
            tx.execute(
                "INSERT INTO inbox(arrival, received_at, kind, lane, our_kind, sender, sender_device,
                   timestamp, sealed, content) VALUES (?1, ?2, 'content', ?3, 'aci', ?4, 1, ?5, 0, ?6)",
                kourier_db::rusqlite::params![arrival, received_at, lane, sender, arrival + 1_000, content],
            )?;
            Ok(())
        })
        .unwrap();
}

fn attempts(fixture: &Fixture, arrival: i64) -> Option<(u32, Option<i64>)> {
    fixture
        .db
        .read(|c| -> crate::error::Result<_> {
            use kourier_db::rusqlite::OptionalExtension as _;
            Ok(c.query_row(
                "SELECT attempts, parked_until FROM inbox WHERE arrival = ?1",
                [arrival],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .unwrap()
}

#[test]
fn content_failures_are_retried_then_dropped_with_a_placeholder() {
    let fixture = fixture();
    let sender = ServiceId::from(Aci::from(Uuid::new_v4())).service_id_string();
    insert_row(&fixture, 1, &sender, &sender, &[0x0a, 0xff], 0);
    // A good row behind it in the same lane is not held back.
    let good = text_content("next", 1_002);
    insert_row(&fixture, 2, &sender, &sender, &good.encode_to_vec(), 0);
    let mut now = 10;
    stage2_all(&fixture, now);
    let events = outbox_events(&fixture.db);
    assert_eq!(messages(&events).len(), 1, "the good row passed");
    clear_outbox(&fixture.db);
    for attempt in 1..stage2::MAX_ATTEMPTS {
        let (count, parked) = attempts(&fixture, 1).expect("still there");
        assert_eq!(count, attempt);
        let parked = parked.expect("parked");
        assert!(parked > now);
        // Not due yet: nothing happens.
        stage2_all(&fixture, parked - 1);
        assert_eq!(attempts(&fixture, 1).unwrap().0, attempt);
        now = parked;
        stage2_all(&fixture, now);
    }
    assert_eq!(
        attempts(&fixture, 1),
        None,
        "dropped after the last attempt"
    );
    let events = outbox_events(&fixture.db);
    let [message] = messages(&events)[..] else {
        panic!("one placeholder: {events:?}");
    };
    assert_eq!(
        message.body,
        Body::Placeholder(kourier_model::Placeholder::Undecryptable { resendable: false })
    );
    assert_eq!(&*message.local, "1001");
}

#[test]
fn a_poison_row_from_our_own_account_leaves_no_placeholder() {
    let fixture = fixture();
    let own = ServiceId::from(fixture.account.aci).service_id_string();
    insert_row(&fixture, 1, "misc", &own, &[0x0a, 0xff], 0);
    stage2_all(&fixture, stage2::MAX_AGE_MS + 1);
    assert_eq!(attempts(&fixture, 1), None, "dropped");
    assert!(
        outbox_events(&fixture.db).is_empty(),
        "no undecryptable message from ourselves in note-to-self"
    );
}

#[test]
fn a_failing_row_older_than_45_days_is_dropped_at_once() {
    let fixture = fixture();
    let sender = ServiceId::from(Aci::from(Uuid::new_v4())).service_id_string();
    insert_row(&fixture, 1, &sender, &sender, &[0x0a, 0xff], 0);
    stage2_all(&fixture, stage2::MAX_AGE_MS + 1);
    assert_eq!(attempts(&fixture, 1), None);
    assert_eq!(
        messages(&outbox_events(&fixture.db)).len(),
        1,
        "with a placeholder"
    );
}

/// Network steps that hold back one lane until opened, or fail it.
struct Gate {
    lane: String,
    open: Rc<Notify>,
    fail: bool,
    runs: Rc<std::cell::Cell<u32>>,
}

impl stage2::NetworkSteps for Gate {
    fn run<'a>(&'a self, lane: &'a str, _rows: &'a [stage2::Row]) -> stage2::StepFuture<'a> {
        let held = lane == self.lane;
        if held {
            self.runs.set(self.runs.get() + 1);
        }
        let fail = self.fail;
        let open = self.open.clone();
        Box::pin(async move {
            if !held {
                return Ok(stage2::LaneInputs::default());
            }
            if fail {
                return Err(stage2::NetworkFailure("unreachable".to_owned()));
            }
            open.notified().await;
            Ok(stage2::LaneInputs::default())
        })
    }
}

fn gate(lane: &str, fail: bool) -> (Rc<Gate>, Rc<Notify>) {
    let open = Rc::new(Notify::new());
    (
        Rc::new(Gate {
            lane: lane.to_owned(),
            open: open.clone(),
            fail,
            runs: Rc::default(),
        }),
        open,
    )
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn lanes_keep_order_inside_and_do_not_hold_each_other_back() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fixture = fixture();
            let mut alice = Phone::new(&fixture);
            let mut bob = Phone::new(&fixture);
            let batch = [
                alice.envelope(&fixture, &text_content("a1", T), T),
                alice.envelope(&fixture, &text_content("a2", T + 1), T + 1),
                bob.envelope(&fixture, &text_content("b1", T + 2), T + 2),
            ];
            stage1_all(&fixture, &[], &batch);
            let alice_lane = ServiceId::from(alice.aci).service_id_string();
            let (steps, open) = gate(&alice_lane, false);
            let mut test = crate::actor::testing::shared(
                fixture.db.clone(),
                fixture.account.clone(),
                Vec::new(),
            );
            test.shared.lanes = stage2::Lanes::new(steps);
            let shared = Rc::new(test.shared);
            let stop = tokio_util::sync::CancellationToken::new();
            let driver = tokio::task::spawn_local(stage2::drive(shared.clone(), stop.clone()));

            // Bob's lane runs while Alice's waits for its network step.
            until("bob's message", || {
                messages(&outbox_events(&fixture.db)).len() == 1
            })
            .await;
            assert_eq!(count(&fixture.db, "inbox"), 2, "alice's rows wait");
            open.notify_one();
            until("alice's messages", || count(&fixture.db, "inbox") == 0).await;
            let events = outbox_events(&fixture.db);
            let bodies: Vec<_> = messages(&events).iter().map(|m| m.body.clone()).collect();
            assert_eq!(
                bodies,
                [Body::text("b1"), Body::text("a1"), Body::text("a2")]
            );
            stop.cancel();
            driver.await.unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn network_failures_park_the_lane_and_never_count() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let fixture = fixture();
            let mut alice = Phone::new(&fixture);
            let mut bob = Phone::new(&fixture);
            let batch = [
                alice.envelope(&fixture, &text_content("a1", T), T),
                bob.envelope(&fixture, &text_content("b1", T + 2), T + 2),
            ];
            stage1_all(&fixture, &[], &batch);
            let alice_lane = ServiceId::from(alice.aci).service_id_string();
            let (steps, _open) = gate(&alice_lane, true);
            let runs = steps.runs.clone();
            let mut test = crate::actor::testing::shared(
                fixture.db.clone(),
                fixture.account.clone(),
                Vec::new(),
            );
            test.shared.lanes = stage2::Lanes::new(steps);
            let shared = Rc::new(test.shared);
            assert_eq!(
                stage2::lane_step(&shared, &alice_lane).await.unwrap(),
                stage2::Step::Parked
            );
            assert_eq!(attempts(&fixture, 1), Some((0, None)), "no attempt counted");
            let stop = tokio_util::sync::CancellationToken::new();
            let driver = tokio::task::spawn_local(stage2::drive(shared.clone(), stop.clone()));
            until("bob's message", || {
                messages(&outbox_events(&fixture.db)).len() == 1
            })
            .await;
            // Alice's lane stays parked (5 s): the driver does not retry it at once.
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(runs.get(), 1);
            assert_eq!(attempts(&fixture, 1), Some((0, None)));
            stop.cancel();
            driver.await.unwrap();
        })
        .await;
}

mod number_change;

mod calls_attack;
