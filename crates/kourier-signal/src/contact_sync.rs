//! Contact sync (§6.9): the phone answers `SyncMessage.Request{CONTACTS}` with
//! `SyncMessage.Contacts{blob, complete}`, an `AttachmentPointer` to a stream of
//! varint-length-delimited `ContactDetails`, each optionally followed by its avatar bytes.
//!
//! Stage 2 only queues the blob as a `contact_sync` job (no network inside a lane step); the job
//! downloads and decrypts it in memory (bounded by [`MAX_BLOB_LEN`]), writes the avatars, then
//! applies the contacts in zones of ≤ [`MERGE_BATCH`] together with their `ContactUpsert`s.
//!
//! Desktop sources: `SD/ts/services/contactSync.preload.ts` (`doContactSync`: three download
//! attempts, a full sync clears the names of contacts it no longer lists, the disappearing timer
//! only on the initial sync), `SD/ts/textsecure/ContactsParser.preload.ts`.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_db::Tx;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{
    ContactPatch, ConvKey, ConversationPatch, Identifier, IdentifierKind, Patch, UserId,
};
use libsignal_core::ServiceId;
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use tokio::sync::Notify;
use zeroize::Zeroizing;

use crate::actor::Shared;
use crate::attachment::{self, Pointer};
use crate::avatar::{self, Source};
use crate::db::Account;
use crate::error::{Error, Result};
use crate::jobs::{self, Done, Job, JobKind};
use crate::net::SignalTransport;
use crate::outbox::append_events;
use crate::proto;
use crate::service_id::{aci_from_wire, aci_user_id, validate_e164};
use crate::{features, peers};

/// Largest contact-sync blob accepted.
pub(crate) const MAX_BLOB_LEN: u64 = 64 * 1024 * 1024;
/// Contacts applied per zone.
pub(crate) const MERGE_BATCH: usize = 500;
/// How long one download may take (Desktop: 90 s).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the contact sync answer is waited for before a link reports `Done` without it.
pub(crate) const LINK_WAIT: Duration = Duration::from_secs(60);
/// How long a received contact sync is retried (Desktop gives up after three attempts; the job
/// backoff makes that about a minute, we keep trying for a day like the sync requests).
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Longest varint length prefix.
const MAX_VARINT_LEN: usize = 5;

/// One parsed contact.
pub(crate) struct Parsed {
    /// The details.
    pub(crate) details: proto::ContactDetails,
    /// The avatar bytes that followed them, if any.
    pub(crate) avatar: Option<Zeroizing<Vec<u8>>>,
}

impl std::fmt::Debug for Parsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parsed")
            .field("avatar", &self.avatar.as_ref().map(|a| a.len()))
            .finish_non_exhaustive()
    }
}

/// Why a contact-sync blob did not parse (log-safe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ParseError {
    /// A length prefix is malformed or runs past the end.
    #[error("contact sync: bad length prefix")]
    Length,
    /// A `ContactDetails` did not decode.
    #[error("contact sync: undecodable contact")]
    Details,
    /// An avatar runs past the end of the blob.
    #[error("contact sync: truncated avatar")]
    Avatar,
}

/// Reads a protobuf varint at `*at`.
fn varint(data: &[u8], at: &mut usize) -> Result<u64, ParseError> {
    let mut value = 0u64;
    for i in 0..MAX_VARINT_LEN {
        let byte = *data.get(*at).ok_or(ParseError::Length)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(ParseError::Length)
}

/// Parses the decrypted blob (`ContactsParser.preload.ts`).
pub(crate) fn parse(data: &[u8]) -> Result<Vec<Parsed>, ParseError> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < data.len() {
        let len = usize::try_from(varint(data, &mut at)?).map_err(|_| ParseError::Length)?;
        let end = at.checked_add(len).ok_or(ParseError::Length)?;
        let bytes = data.get(at..end).ok_or(ParseError::Length)?;
        at = end;
        let details = proto::ContactDetails::decode(bytes).map_err(|_| ParseError::Details)?;
        let avatar_len = details
            .avatar
            .as_ref()
            .and_then(|a| a.length)
            .map_or(0, |len| len as usize);
        let avatar = if avatar_len > 0 {
            let end = at.checked_add(avatar_len).ok_or(ParseError::Avatar)?;
            let bytes = data.get(at..end).ok_or(ParseError::Avatar)?;
            at = end;
            Some(Zeroizing::new(bytes.to_vec()))
        } else {
            None
        };
        out.push(Parsed { details, avatar });
    }
    Ok(out)
}

/// Stage 2: queues a received `SyncMessage.Contacts` (a newer one replaces a queued one).
pub(crate) fn enqueue(
    conn: &Connection,
    contacts: &proto::sync_message::Contacts,
    now: i64,
) -> Result<()> {
    jobs::enqueue_payload(
        conn,
        JobKind::ContactSync,
        "",
        &contacts.encode_to_vec(),
        now,
        Some(MAX_AGE),
    )
}

/// The `contact_sync` job: download, decrypt, parse, write avatars, apply.
pub(crate) async fn run<T: SignalTransport>(shared: &Rc<Shared<T>>, job: &Job) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let contacts = job
        .payload
        .as_deref()
        .and_then(|p| proto::sync_message::Contacts::decode(p).ok());
    let Some(contacts) = contacts else {
        tracing::warn!("contact sync job without a valid payload dropped");
        return Ok(Done::Finished);
    };
    let pointer = contacts
        .blob
        .as_ref()
        .ok_or(attachment::AttachmentError::Incomplete("blob"))
        .and_then(Pointer::from_proto);
    let pointer = match pointer {
        Ok(pointer) => pointer,
        Err(e) => {
            tracing::warn!(error = %e, "contact sync without a usable blob");
            shared.db.zone(|tx| mark_done(tx))?;
            return Ok(Done::Finished);
        }
    };
    let data =
        attachment::download_in_memory(&shared.net, &pointer, MAX_BLOB_LEN, DOWNLOAD_TIMEOUT)
            .await?;
    let parsed = parse(&data).map_err(|_| Error::Decode("contact sync blob"))?;
    let complete = contacts.complete.unwrap_or(false);
    apply(shared, &account, &parsed, complete)?;
    tracing::info!(contacts = parsed.len(), complete, "contact sync applied");
    Ok(Done::Finished)
}

/// One contact of the blob, checked.
struct Contact<'a> {
    user: UserId,
    sid: String,
    name: Option<String>,
    number: Option<&'a str>,
    expire_timer: Option<u32>,
    /// The version of that timer (Desktop: 1 when the phone states none).
    expire_timer_version: u32,
    avatar: Option<&'a [u8]>,
}

fn checked<'a>(account: &Account, parsed: &'a Parsed) -> Option<Contact<'a>> {
    let details = &parsed.details;
    let aci = aci_from_wire(details.aci_binary.as_deref(), details.aci.as_deref())
        .ok()
        .flatten()?;
    if aci == account.aci {
        return None;
    }
    let user = aci_user_id(aci).ok()?;
    Some(Contact {
        user,
        sid: ServiceId::from(aci).service_id_string(),
        name: details
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned),
        number: details
            .number
            .as_deref()
            .filter(|n| validate_e164(n).is_ok()),
        expire_timer: details.expire_timer.filter(|t| *t > 0),
        expire_timer_version: details.expire_timer_version.unwrap_or(1).max(1),
        avatar: parsed.avatar.as_deref().map(Vec::as_slice),
    })
}

/// Writes the avatars that changed (outside any zone), then applies the contacts zone by zone.
fn apply<T: SignalTransport>(
    shared: &Shared<T>,
    account: &Account,
    parsed: &[Parsed],
    complete: bool,
) -> Result<()> {
    let contacts: Vec<Contact<'_>> = parsed.iter().filter_map(|p| checked(account, p)).collect();
    // Files first: the zones only record them (`crate::avatar`). An address-book photo
    // replaces a profile picture only when the phone's setting prefers it
    // (`AccountRecord.preferContactAvatars`).
    let prefer_contact = shared.db.read(crate::storage::prefer_contact_avatars)?;
    let mut written: HashMap<String, (avatar::Written, [u8; 32])> = HashMap::new();
    for contact in &contacts {
        let Some(image) = contact.avatar else {
            continue;
        };
        if image.len() as u64 > avatar::MAX_AVATAR_LEN {
            continue;
        }
        let hash: [u8; 32] = Sha256::digest(image).into();
        let (stored_hash, source) = shared.db.read(|c| avatar_state(c, &contact.sid))?;
        if !takes_contact_photo(
            source,
            stored_hash.as_deref() == Some(&hash[..]),
            prefer_contact,
        ) {
            continue;
        }
        written.insert(
            contact.sid.clone(),
            (avatar::write(&shared.blob_dir, image)?, hash),
        );
    }
    let zones = contacts.len().div_ceil(MERGE_BATCH).max(1);
    let mut chunks = contacts.chunks(MERGE_BATCH);
    for index in 0..zones {
        let batch = chunks.next().unwrap_or(&[]);
        let last = index + 1 == zones;
        let now = shared.now();
        let result = shared.db.zone(|tx| {
            apply_zone(
                tx,
                account,
                &shared.blob_dir,
                batch,
                &written,
                last.then_some(complete),
                now,
                &shared.wake,
            )
        });
        if let Err(e) = result {
            // Files of this and later zones are recorded nowhere; earlier zones keep theirs.
            for contact in &contacts[index * MERGE_BATCH..] {
                if let Some((file, _)) = written.get(&contact.sid) {
                    avatar::discard(&shared.blob_dir, file);
                }
            }
            return Err(e);
        }
        kourier_db::failpoint::hit(crate::receive::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
    }
    Ok(())
}

/// Whether the address-book photo of a contact sync becomes the contact's avatar: not the one
/// already shown (`same`), and not over a profile picture unless the phone's setting prefers
/// address-book photos (Desktop's `getAvatar`: `preferContactAvatars`).
pub(crate) fn takes_contact_photo(shown: Option<Source>, same: bool, prefer_contact: bool) -> bool {
    !same && (prefer_contact || shown != Some(Source::Profile))
}

fn avatar_state(conn: &Connection, sid: &str) -> Result<(Option<Vec<u8>>, Option<Source>)> {
    let row: Option<(Option<Vec<u8>>, Option<String>)> = conn
        .prepare_cached("SELECT avatar_hash, avatar_source FROM peers WHERE service_id = ?1")?
        .query_row([sid], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (hash, source) = row.unwrap_or((None, None));
    let source = match source.as_deref() {
        Some("profile") => Some(Source::Profile),
        Some("contact_sync") => Some(Source::ContactSync),
        _ => None,
    };
    Ok((hash, source))
}

/// The link waits for this (§6.5 step 8); the deadline job is no longer needed.
fn mark_done(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE account SET contact_sync_done = 1 WHERE id = 1", [])?;
    conn.execute(
        "DELETE FROM jobs WHERE kind = ?1",
        [JobKind::ContactSyncWait.as_str()],
    )?;
    Ok(())
}

/// One zone of contacts. `last` is `Some(complete)` for the last zone of a sync.
#[allow(clippy::too_many_arguments, reason = "one zone's inputs, called once")]
fn apply_zone(
    tx: &mut Tx<'_>,
    account: &Account,
    blob_dir: &std::path::Path,
    batch: &[Contact<'_>],
    written: &HashMap<String, (avatar::Written, [u8; 32])>,
    last: Option<bool>,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<()> {
    let initial: bool = tx.query_row(
        "SELECT contact_sync_done = 0 FROM account WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    let mut events = Vec::with_capacity(batch.len() + 1);
    for contact in batch {
        let before = peers::load(tx, &contact.sid)?;
        let mut patch = ContactPatch::new(contact.user.clone());
        if let Some(name) = &contact.name {
            patch.system_name = Patch::Set(name.clone());
            peers::ensure(tx, &contact.sid, now)?;
            tx.prepare_cached("UPDATE peers SET in_contact_sync = 2 WHERE service_id = ?1")?
                .execute([&contact.sid])?;
        }
        if let Some(number) = contact.number {
            patch.identifiers = Some(vec![Identifier {
                kind: IdentifierKind::Phone,
                value: number.to_owned(),
            }]);
        }
        if let Some((file, hash)) = written.get(&contact.sid) {
            patch.avatar = avatar::record(
                tx,
                blob_dir,
                &contact.sid,
                &avatar::Change {
                    written: Some(file),
                    source: Source::ContactSync,
                    profile_path: None,
                    hash: Some(&hash[..]),
                },
                now,
            )?;
        }
        events.push(BackendEvent::ContactUpsert(patch));
        let after = peers::load(tx, &contact.sid)?;
        let has_chat = after.features_hash.is_some();
        let mut conv = ConversationPatch::new(ConvKey::Direct(contact.user.clone()));
        let mut touch = has_chat && before.accepted() != after.accepted();
        // "expireTimer isn't in Storage Service": the initial sync states each chat's timer
        // and its version (`SD/ts/services/contactSync.preload.ts`). The backend keeps both for
        // what it sends (`crate::timer`); the core hears of it now when it has the chat (an
        // empty chat per contact would flood the list), else with the chat's first event
        // (`features::features_event`).
        if initial {
            peers::reset_expire_timer(
                tx,
                &contact.sid,
                contact.expire_timer.unwrap_or(0),
                contact.expire_timer_version,
                now,
            )?;
            if has_chat && contact.expire_timer.unwrap_or(0) != before.expire_timer {
                conv.timer = contact.expire_timer.map_or(Patch::Clear, |secs| {
                    Patch::Set(Duration::from_secs(u64::from(secs)))
                });
                conv.timer_version = Some(contact.expire_timer_version);
                touch = true;
            }
        }
        if touch {
            events.push(BackendEvent::ConversationUpsert(conv));
        }
    }
    if let Some(complete) = last {
        if complete {
            // A full sync: contacts it no longer names lose their address-book name.
            let gone: Vec<String> = tx
                .prepare_cached("SELECT service_id FROM peers WHERE in_contact_sync = 1")?
                .query_map([], |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            for sid in gone {
                let Some(user) = ServiceId::parse_from_service_id_string(&sid)
                    .and_then(|s| crate::service_id::service_id_user_id(s).ok())
                else {
                    continue;
                };
                let before = peers::load(tx, &sid)?;
                tx.prepare_cached("UPDATE peers SET in_contact_sync = 0 WHERE service_id = ?1")?
                    .execute([&sid])?;
                let after = peers::load(tx, &sid)?;
                let mut patch = ContactPatch::new(user.clone());
                patch.system_name = Patch::Clear;
                events.push(BackendEvent::ContactUpsert(patch));
                // Out of the address book: a chat the core has may no longer be accepted
                // (`with_features` sends its new features).
                if after.features_hash.is_some() && before.accepted() != after.accepted() {
                    events.push(BackendEvent::ConversationUpsert(ConversationPatch::new(
                        ConvKey::Direct(user),
                    )));
                }
            }
            tx.execute(
                "UPDATE peers SET in_contact_sync = 0 WHERE in_contact_sync = 1",
                [],
            )?;
        }
        tx.execute(
            "UPDATE peers SET in_contact_sync = 1 WHERE in_contact_sync = 2",
            [],
        )?;
        mark_done(tx)?;
    }
    let events = features::with_features(tx, account, events, now)?;
    if !events.is_empty() {
        append_events(tx, &events, now, wake)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
