//! Our phone changed its number (§6.6, §6.14 item 13): `SyncMessage.pniChangeNumber` gives this
//! device its new phone number identity (PNI), that identity's key pair, registration id, signed
//! prekey and Kyber last-resort key, and the new number.
//!
//! It is applied inside the stage-1 zone, in the savepoint of the envelope that carried it,
//! because later envelopes of the same batch may already be addressed to the new PNI (Desktop
//! handles it "in the middle of decryption queue since subsequent envelopes use their key
//! material", `SD/ts/textsecure/MessageReceiver.preload.ts:1585-1596`, `#handlePNIChangeNumber`,
//! `AccountManager.preload.ts` `setPni`, `SignalProtocolStore.preload.ts` `removeOurOldPni` and
//! `updateOurPniKeyMaterial`).
//!
//! Everything changes in that one zone or not at all: the account row, the PNI identity, the
//! keys, what depended on the old PNI (its prekeys and sessions, the sender certificates with
//! the old number, group credentials, "they know my number" marks), the job that uploads fresh
//! PNI prekeys, and the `AccountReady` that tells the core our new ids.

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use libsignal_core::{Pni, ServiceId};
use libsignal_protocol::{
    GenericSignedPreKey as _, IdentityKeyPair, KyberPreKeyRecord, SignedPreKeyRecord,
};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::db::{Account, KeyIds};
use crate::error::Result;
use crate::jobs::{self, JobKind};
use crate::proto;
use crate::service_id::validate_e164;
use crate::store::{IdentityKind, insert_kyber_prekey, insert_signed_prekey, save_own_identity};

/// The `jobs.key` of the key maintenance run a number change asks for: new one-time PNI
/// prekeys, and a signed prekey and last-resort key of our own making (Desktop's
/// `maybeUpdateKeys(PNI, forceUpdate)`).
pub(crate) const FORCED_PNI_KEYS: &str = "pni";

/// How long that upload is retried.
const FORCED_UPLOAD_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// A validated `PniChangeNumber`.
pub(crate) struct NumberChange {
    pni: Pni,
    identity: IdentityKeyPair,
    signed: SignedPreKeyRecord,
    last_resort: Option<KyberPreKeyRecord>,
    registration_id: u32,
    e164: String,
}

impl std::fmt::Debug for NumberChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The number and the keys never print (§9.1).
        f.debug_struct("NumberChange")
            .field("pni", &crate::service_id::SignalUser::Pni(self.pni))
            .field("last_resort", &self.last_resort.is_some())
            .finish_non_exhaustive()
    }
}

/// The new PNI an envelope announces (`updatedPniBinary`: 16 bytes; `updatedPni`: a uuid string,
/// with or without the `PNI:` tag).
pub(crate) fn updated_pni(envelope: &proto::Envelope) -> Option<Pni> {
    if let Some(bytes) = envelope
        .updated_pni_binary
        .as_deref()
        .filter(|b| !b.is_empty())
    {
        return match ServiceId::parse_from_service_id_binary(bytes) {
            Some(ServiceId::Pni(pni)) => Some(pni),
            Some(ServiceId::Aci(_)) | None => Uuid::from_slice(bytes).ok().map(Pni::from),
        };
    }
    let text = envelope.updated_pni.as_deref().filter(|s| !s.is_empty())?;
    let untagged = text.strip_prefix("PNI:").unwrap_or(text);
    Uuid::try_parse(untagged).ok().map(Pni::from)
}

/// Checks a `PniChangeNumber` and the envelope's new PNI. `Err` names what is wrong (log-safe);
/// such a message changes nothing (Desktop: "empty pni change number sync message").
///
/// Stricter than Desktop, as the provisioning check is (§13 phase A): the signed prekey and the
/// last-resort key must be signed by the identity they come with. A device that stored keys the
/// identity did not sign would answer every PreKey message to its number with a failure.
pub(crate) fn validate(
    pni: Option<Pni>,
    change: &proto::sync_message::PniChangeNumber,
) -> std::result::Result<NumberChange, &'static str> {
    let pni = pni.ok_or("no new PNI on the envelope")?;
    let identity = change
        .identity_key_pair
        .as_deref()
        .filter(|b| !b.is_empty())
        .ok_or("no identity key pair")?;
    let identity =
        IdentityKeyPair::try_from(identity).map_err(|_| "malformed identity key pair")?;
    let derived = identity
        .private_key()
        .public_key()
        .map_err(|_| "malformed identity private key")?;
    if derived != *identity.public_key() {
        return Err("identity key pair does not match");
    }
    let signed = change
        .signed_pre_key
        .as_deref()
        .filter(|b| !b.is_empty())
        .ok_or("no signed prekey")?;
    let signed = SignedPreKeyRecord::deserialize(signed).map_err(|_| "malformed signed prekey")?;
    let signed_ok = signed
        .public_key()
        .ok()
        .zip(signed.signature().ok())
        .is_some_and(|(key, signature)| {
            identity
                .public_key()
                .verify_signature(&key.serialize(), &signature)
        });
    if !signed_ok {
        return Err("signed prekey not signed by the new identity");
    }
    let last_resort = change
        .last_resort_kyber_pre_key
        .as_deref()
        .filter(|b| !b.is_empty())
        .map(|bytes| KyberPreKeyRecord::deserialize(bytes).map_err(|_| "malformed Kyber prekey"))
        .transpose()?;
    if let Some(kyber) = &last_resort {
        let kyber_ok = kyber
            .public_key()
            .ok()
            .zip(kyber.signature().ok())
            .is_some_and(|(key, signature)| {
                identity
                    .public_key()
                    .verify_signature(&key.serialize(), &signature)
            });
        if !kyber_ok {
            return Err("Kyber prekey not signed by the new identity");
        }
    }
    let registration_id = change.registration_id.ok_or("no registration id")?;
    // 14 bits, as every registration id (§6.5 step 4).
    if !(1..=0x3FFF).contains(&registration_id) {
        return Err("registration id out of range");
    }
    let e164 = change.new_e164.clone().ok_or("no new number")?;
    validate_e164(&e164).map_err(|_| "malformed new number")?;
    Ok(NumberChange {
        pni,
        identity,
        signed,
        last_resort,
        registration_id,
        e164,
    })
}

/// Applies a validated number change on `conn` (inside a zone) and returns the account as it
/// is now, and the `AccountReady` for the core. Applying the same change again gives the same
/// state.
pub(crate) fn apply(
    conn: &Connection,
    account: &Account,
    change: &NumberChange,
    now: i64,
) -> Result<(Account, BackendEvent)> {
    let kind = IdentityKind::Pni;
    let old_pni = ServiceId::from(account.pni).service_id_string();
    let new_pni = ServiceId::from(change.pni).service_id_string();

    // Everything of the old PNI goes (`removeOurOldPni`), including what Kourier keys by "our
    // PNI" rather than by its uuid: sessions others opened to our old number, and the
    // last-resort triples of the old keys.
    for table in [
        "prekeys",
        "signed_prekeys",
        "kyber_prekeys",
        "kyber_triples",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE identity = ?1"),
            [kind.as_str()],
        )?;
    }
    conn.execute("DELETE FROM sessions WHERE our_kind = ?1", [kind.as_str()])?;
    if old_pni != new_pni {
        conn.execute("DELETE FROM identities WHERE service_id = ?1", [&old_pni])?;
    }

    // The new identity and its keys (`updateOurPniKeyMaterial`). The phone already gave both
    // keys to the server for us: they are confirmed, with the time they were made.
    let mut ids = KeyIds::load(conn, kind)?;
    ids.signed = KeyIds::advance(u32::from(change.signed.id()?), 1);
    if let Some(kyber) = &change.last_resort {
        ids.kyber = KeyIds::advance(u32::from(kyber.id()?), 1);
    }
    conn.execute(
        "UPDATE own_identity SET public_key = ?2, private_key = ?3, next_prekey_id = ?4,
           next_signed_prekey_id = ?5, next_kyber_prekey_id = ?6 WHERE kind = ?1",
        (
            kind.as_str(),
            &change.identity.identity_key().serialize()[..],
            Zeroizing::new(change.identity.private_key().serialize()).as_slice(),
            ids.prekey,
            ids.signed,
            ids.kyber,
        ),
    )?;
    save_own_identity(conn, &new_pni, change.identity.identity_key(), now)?;
    insert_signed_prekey(conn, kind, &change.signed, true)?;
    if let Some(kyber) = &change.last_resort {
        let created_at = kyber
            .timestamp()
            .map(|t| i64::try_from(t.epoch_millis()).unwrap_or(i64::MAX))
            .unwrap_or(now);
        insert_kyber_prekey(conn, kind, kyber, true, true, created_at)?;
    }

    conn.execute(
        "UPDATE account SET pni = ?1, e164 = ?2, pni_registration_id = ?3 WHERE id = 1",
        (
            Uuid::from(change.pni).hyphenated().to_string(),
            &change.e164,
            change.registration_id,
        ),
    )?;

    // What was made for the old number: nobody has been told the new one
    // (`clearShareMyPhoneNumber`), the sender certificate with the number names the old one,
    // and group credentials are issued for (ACI, PNI) ("PNI has changed and credentials are no
    // longer valid").
    conn.execute(
        "UPDATE peers SET share_my_number = 0, updated_at = ?1 WHERE share_my_number = 1",
        [now],
    )?;
    conn.execute("DELETE FROM sender_certs", [])?;
    conn.execute("DELETE FROM group_credentials", [])?;

    // Fresh PNI prekeys of our own (the server has none for the new PNI yet).
    jobs::enqueue(
        conn,
        JobKind::KeyMaintenance,
        FORCED_PNI_KEYS,
        now,
        Some(FORCED_UPLOAD_MAX_AGE),
    )?;

    let mut changed = account.clone();
    changed.pni = change.pni;
    changed.e164.clone_from(&change.e164);
    changed.pni_registration_id = change.registration_id;
    changed.pni_identity = change.identity;
    let ready = BackendEvent::AccountReady {
        own_ids: changed.own_ids()?,
        device: Some(u32::from(changed.device_id)),
        display_name: None,
        // The new number reaches the core with the new ids.
        number: changed.number(),
    };
    Ok((changed, ready))
}

#[cfg(test)]
mod tests {
    use libsignal_protocol::Timestamp;

    use super::*;
    use crate::keys::{generate_kyber_prekey, generate_signed_prekey};

    fn change(identity: &IdentityKeyPair) -> proto::sync_message::PniChangeNumber {
        let rng = &mut rand::rng();
        let signed =
            generate_signed_prekey(identity, 7, Timestamp::from_epoch_millis(1_000), rng).unwrap();
        let kyber = generate_kyber_prekey(identity, 9).unwrap();
        proto::sync_message::PniChangeNumber {
            identity_key_pair: Some(identity.serialize().to_vec()),
            signed_pre_key: Some(signed.serialize().unwrap()),
            last_resort_kyber_pre_key: Some(kyber.serialize().unwrap()),
            registration_id: Some(4242),
            new_e164: Some("+15550123".to_owned()),
        }
    }

    #[test]
    fn the_new_pni_is_read_from_either_envelope_field() {
        let uuid = Uuid::from_u128(0x1234_5678_9abc_def0);
        let pni = Pni::from(uuid);
        let binary = proto::Envelope {
            updated_pni_binary: Some(uuid.as_bytes().to_vec()),
            ..proto::Envelope::default()
        };
        assert_eq!(updated_pni(&binary), Some(pni));
        for text in [uuid.hyphenated().to_string(), format!("PNI:{uuid}")] {
            let string = proto::Envelope {
                updated_pni: Some(text),
                ..proto::Envelope::default()
            };
            assert_eq!(updated_pni(&string), Some(pni));
        }
        assert_eq!(updated_pni(&proto::Envelope::default()), None);
        let junk = proto::Envelope {
            updated_pni: Some("not a uuid".to_owned()),
            ..proto::Envelope::default()
        };
        assert_eq!(updated_pni(&junk), None);
    }

    #[test]
    fn a_change_is_refused_unless_whole_and_signed_by_its_identity() {
        let rng = &mut rand::rng();
        let identity = IdentityKeyPair::generate(rng);
        let pni = Some(Pni::from(Uuid::from_u128(5)));
        let good = change(&identity);
        let valid = validate(pni, &good).unwrap();
        assert_eq!(valid.registration_id, 4242);
        assert!(valid.last_resort.is_some());
        // Nothing of it prints.
        let debug = format!("{valid:?}");
        assert!(!debug.contains("5550123"), "{debug}");

        assert_eq!(
            validate(None, &good).unwrap_err(),
            "no new PNI on the envelope"
        );
        let without = |edit: fn(&mut proto::sync_message::PniChangeNumber)| {
            let mut broken = change(&identity);
            edit(&mut broken);
            validate(pni, &broken).unwrap_err()
        };
        assert_eq!(
            without(|c| c.identity_key_pair = None),
            "no identity key pair"
        );
        assert_eq!(
            without(|c| c.identity_key_pair = Some(vec![1, 2, 3])),
            "malformed identity key pair"
        );
        assert_eq!(without(|c| c.signed_pre_key = None), "no signed prekey");
        assert_eq!(without(|c| c.registration_id = None), "no registration id");
        assert_eq!(
            without(|c| c.registration_id = Some(0x4000)),
            "registration id out of range"
        );
        assert_eq!(without(|c| c.new_e164 = None), "no new number");
        assert_eq!(
            without(|c| c.new_e164 = Some("5550123".to_owned())),
            "malformed new number"
        );
        // The last-resort key is optional (Desktop's `dropNull`).
        let mut optional = change(&identity);
        optional.last_resort_kyber_pre_key = None;
        assert!(validate(pni, &optional).unwrap().last_resort.is_none());

        // Keys of another identity: a hostile or broken sync message.
        let other = IdentityKeyPair::generate(rng);
        let mut forged = change(&other);
        forged.identity_key_pair = Some(identity.serialize().to_vec());
        assert_eq!(
            validate(pni, &forged).unwrap_err(),
            "signed prekey not signed by the new identity"
        );
        let mut forged_kyber = change(&identity);
        forged_kyber.last_resort_kyber_pre_key = change(&other).last_resort_kyber_pre_key;
        assert_eq!(
            validate(pni, &forged_kyber).unwrap_err(),
            "Kyber prekey not signed by the new identity"
        );
    }
}
