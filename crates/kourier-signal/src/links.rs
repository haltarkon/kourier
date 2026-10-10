//! Which ACI a PNI belongs to (§3.3, §6.13): the one place that records a pair this device
//! learned and tells the core, with `IdentityMerged{pni → aci}`, when it is new.
//!
//! A contact our phone knows only by number is a PNI to us: the phone's transcript of a
//! message to them makes a chat keyed by that PNI. Their answers come from their ACI, as do
//! their receipts, reactions, edits and deletes, and our phone names the chat by the ACI in
//! its sync messages once it knows it. Until the core has been told that the two ids are one
//! person, those events name a chat the message is not in, wait in the core
//! (`pending_modifiers`) and are lost when they waited too long. So every source of a pair
//! reports here, whichever comes first:
//!
//! - a Storage Service contact record that carries both ids (`storage::merge_contact`; our
//!   phone writes it when it merges the contact),
//! - a `PniSignatureMessage` that verifies (`receive::stage1`; the PNI identity key signed the
//!   ACI identity key),
//! - a sealed sender's certificate that states the number of an ACI while a contact record
//!   gives that number to a PNI without an ACI ([`number_of_aci`]; Desktop joins the two the
//!   same way, `ConversationController.maybeMergeContacts` on `EnvelopeUnsealed`).
//!
//! Not a source: a group change that promotes an invited PNI to its ACI
//! (`PromoteMemberPendingPniAciProfileKeyAction`). Only the server vouches for that pair, and
//! a merge moves a 1:1 chat to another account; Desktop uses the pair for the wording of the
//! group notice and nothing else (`SD/ts/groups.preload.ts` `promotedAciToPniMap`).
//!
//! The pair is remembered (`identity_links`), so a source that repeats it (every message of a
//! contact carries the PNI signature until our receipt arrives) tells the core once, and a
//! rebuilt `core.db` is told every pair again ([`all`]).

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, OptionalExtension as _};
use libsignal_core::{Aci, Pni, ServiceId};

use crate::db::Account;
use crate::error::Result;
use crate::service_id::{aci_user_id, pni_user_id, validate_e164};

/// `IdentityMerged{pni → aci}`.
fn merged(pni: Pni, aci: Aci) -> Option<BackendEvent> {
    match (pni_user_id(pni), aci_user_id(aci)) {
        (Ok(from), Ok(into)) => Some(BackendEvent::IdentityMerged { from, into }),
        _ => None,
    }
}

/// Records that `pni` belongs to `aci` and returns the event that tells the core, when the
/// pair is new or the PNI moved to another ACI (a number that changed hands). Our own ids are
/// never a contact's. The caller appends the event in the same zone.
pub(crate) fn learn(
    conn: &Connection,
    account: &Account,
    pni: Pni,
    aci: Aci,
    now: i64,
) -> Result<Option<BackendEvent>> {
    if aci == account.aci || pni == account.pni {
        return Ok(None);
    }
    let pni_sid = ServiceId::from(pni).service_id_string();
    let aci_sid = ServiceId::from(aci).service_id_string();
    let known: Option<String> = conn
        .prepare_cached("SELECT aci FROM identity_links WHERE pni = ?1")?
        .query_row([&pni_sid], |r| r.get(0))
        .optional()?;
    if known.as_deref() == Some(aci_sid.as_str()) {
        return Ok(None);
    }
    conn.prepare_cached(
        "INSERT INTO identity_links(pni, aci, learned_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(pni) DO UPDATE SET aci = excluded.aci, learned_at = excluded.learned_at",
    )?
    .execute((&pni_sid, &aci_sid, now))?;
    tracing::debug!(
        relinked = known.is_some(),
        "a contact's PNI and ACI were joined"
    );
    Ok(merged(pni, aci))
}

/// Every pair this device knows, as events: a rebuilt `core.db` has lost its aliases.
pub(crate) fn all(conn: &Connection) -> Result<Vec<BackendEvent>> {
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT pni, aci FROM identity_links ORDER BY learned_at, pni")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(pni, aci)| {
            match (
                ServiceId::parse_from_service_id_string(&pni)?,
                ServiceId::parse_from_service_id_string(&aci)?,
            ) {
                (ServiceId::Pni(pni), ServiceId::Aci(aci)) => merged(pni, aci),
                _ => None,
            }
        })
        .collect())
}

/// A contact record gives `number` to `pni` and names no ACI: remembered (in `peers.e164` of
/// the PNI's row), so that the ACI can be joined when a sealed sender's certificate states the
/// same number ([`number_of_aci`]).
pub(crate) fn number_of_pni(conn: &Connection, pni: Pni, number: &str, now: i64) -> Result<()> {
    if validate_e164(number).is_err() {
        return Ok(());
    }
    conn.prepare_cached(
        "INSERT INTO peers(service_id, e164, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(service_id) DO UPDATE SET e164 = excluded.e164,
           updated_at = excluded.updated_at
         WHERE peers.e164 IS NOT excluded.e164",
    )?
    .execute((ServiceId::from(pni).service_id_string(), number, now))?;
    Ok(())
}

/// A sealed sender's certificate states that `aci` has `number` right now (the server signed
/// it, and the envelope it came with passed its validation). If a contact record gave that
/// number to a PNI, the two are one contact: the pair is [learned](learn) and the event
/// returned. With several PNIs under the number (a stale record) nothing is joined.
pub(crate) fn number_of_aci(
    conn: &Connection,
    account: &Account,
    aci: Aci,
    number: &str,
    now: i64,
) -> Result<Option<BackendEvent>> {
    if validate_e164(number).is_err() {
        return Ok(None);
    }
    let holders: Vec<String> = conn
        .prepare_cached("SELECT service_id FROM peers WHERE e164 = ?1 AND service_id LIKE 'PNI:%'")?
        .query_map([number], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let [holder] = &holders[..] else {
        return Ok(None);
    };
    match ServiceId::parse_from_service_id_string(holder) {
        Some(ServiceId::Pni(pni)) => learn(conn, account, pni, aci, now),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::receive::tests::fixture;

    fn pair(event: &BackendEvent) -> (String, String) {
        match event {
            BackendEvent::IdentityMerged { from, into } => {
                (from.as_str().to_owned(), into.as_str().to_owned())
            }
            other => panic!("{other:?}"),
        }
    }

    /// A pair is told once, whichever source repeats it; a PNI that moved to another account
    /// is told again; our own ids never merge; a rebuilt core gets every pair.
    #[test]
    fn a_pair_is_told_once_and_again_to_a_rebuilt_core() {
        let fixture = fixture();
        let account = &fixture.account;
        let aci = Aci::from(Uuid::from_u128(0xa11ce));
        let pni = Pni::from(Uuid::from_u128(0xa11ce0));
        let other = Aci::from(Uuid::from_u128(0xb0b));
        fixture
            .db
            .zone(|tx| -> Result<()> {
                let first = learn(tx, account, pni, aci, 5)?.expect("new pair");
                assert_eq!(
                    pair(&first),
                    (
                        pni_user_id(pni).unwrap().as_str().to_owned(),
                        aci_user_id(aci).unwrap().as_str().to_owned()
                    ),
                    "the PNI is the alias, the ACI the contact"
                );
                assert_eq!(learn(tx, account, pni, aci, 6)?, None, "known");
                assert_eq!(learn(tx, account, pni, aci, 7)?, None);
                // The number went to another account: its PNI now belongs to that ACI.
                let moved = learn(tx, account, pni, other, 8)?.expect("moved");
                assert_eq!(pair(&moved).1, aci_user_id(other).unwrap().as_str());
                // Our own ids are no contact's.
                let stranger = Pni::from(Uuid::from_u128(0xdead));
                assert_eq!(learn(tx, account, stranger, account.aci, 9)?, None);
                assert_eq!(learn(tx, account, account.pni, aci, 9)?, None);
                let second = Pni::from(Uuid::from_u128(0xc0c0));
                learn(tx, account, second, aci, 10)?.expect("another PNI");
                let again: Vec<(String, String)> = all(tx)?.iter().map(pair).collect();
                assert_eq!(again.len(), 2, "{again:?}");
                assert_eq!(again[0], pair(&moved));
                Ok(())
            })
            .unwrap();
    }

    /// A number a contact record gave to a PNI and a sealed sender's certificate gives to an
    /// ACI joins the two; a number two PNIs hold joins nothing.
    #[test]
    fn a_number_shared_by_a_pni_and_a_certificate_joins_them() {
        let fixture = fixture();
        let account = &fixture.account;
        let aci = Aci::from(Uuid::from_u128(0xa11ce));
        let pni = Pni::from(Uuid::from_u128(0xa11ce0));
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert_eq!(
                    number_of_aci(tx, account, aci, "+14155550101", 1)?,
                    None,
                    "no PNI has the number"
                );
                number_of_pni(tx, pni, "+14155550101", 2)?;
                number_of_pni(tx, pni, "not a number", 2)?;
                assert_eq!(number_of_aci(tx, account, aci, "+14155550199", 3)?, None);
                let event =
                    number_of_aci(tx, account, aci, "+14155550101", 4)?.expect("the same number");
                assert_eq!(
                    pair(&event),
                    (
                        pni_user_id(pni).unwrap().as_str().to_owned(),
                        aci_user_id(aci).unwrap().as_str().to_owned()
                    )
                );
                assert_eq!(number_of_aci(tx, account, aci, "+14155550101", 5)?, None);
                // A second PNI under the same number: one of the records is stale, and which
                // one cannot be told.
                let stale = Pni::from(Uuid::from_u128(0x57a1e));
                number_of_pni(tx, stale, "+14155550101", 6)?;
                let bob = Aci::from(Uuid::from_u128(0xb0b));
                assert_eq!(number_of_aci(tx, account, bob, "+14155550101", 7)?, None);
                Ok(())
            })
            .unwrap();
    }
}
