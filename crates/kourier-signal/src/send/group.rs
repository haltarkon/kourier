//! Group send (§6.7 step 5; ports `SD/ts/util/sendToGroup.preload.ts` `sendToGroupViaSenderKey`
//! and `sendContentMessageToGroup`, the group branch of
//! `SD/ts/jobs/helpers/sendNormalMessage.preload.ts` and `getMessageRecipients`).
//!
//! [`send_to_group`] sends one `Content` to the members of a group, restarting from the top
//! (at most [`MAX_RESTARTS`] times) after every state change, as Desktop does:
//!
//! 1. Recipients: the members minus us, blocked and unregistered contacts, restricted to the
//!    request's `only`; members the server already took this message for (`send_progress`)
//!    count as delivered.
//! 2. Endorsements (`groups::endorsements`): outside now+2 h … now+2 d, or not matching the
//!    member list, the group is refreshed first (once per send).
//! 3. Our chain (`send::sender_key`): started on first use, reset when older than
//!    `desktop.senderKeyMaxAge`.
//! 4. Sessions: a prekey fetch for recipients without a usable session, up to
//!    [`PREKEY_FETCHES_AT_ONCE`] at once after the sender certificate is cached (steps 6 and 9
//!    make sure of the certificate before their sends too: one fetch, never one per member).
//! 5. Partition: members with an endorsement and a session trusted for sending go the
//!    sender-key way; the rest (and everyone when fewer than two qualify, or without
//!    endorsements) get 1:1 sends, where an unconfirmed safety-number change fails that member
//!    with `UntrustedIdentity` as in a 1:1 chat.
//! 6. Member devices: a device that has our chain but is not in a full send means a member left:
//!    the chain is reset; a device without our chain gets our distribution message first (1:1,
//!    up to [`MEMBER_SENDS_AT_ONCE`] members at once);
//!    others that are gone are forgotten.
//! 7. One zone: `group_encrypt`, the sealed content with the certificate without our number and
//!    the group id, `sealed_sender_multi_recipient_encrypt`; then the multi-recipient send
//!    authorized by a `GroupSendFullToken` for exactly those recipients.
//! 8. Answers: accepted → `send_progress` and the Message Send Log (with the group id) in one
//!    zone, unregistered recipients marked; 409/410 → repair the device lists and restart;
//!    401 or an answer the typed API does not know (404) → every member gets a 1:1 send;
//!    connectivity, rate limits and server errors fail the sender-key recipients.
//! 9. 1:1 sends to the rest (up to [`MEMBER_SENDS_AT_ONCE`] at once), sealed with the access
//!    key, or the member's endorsement token when sealed sender is off for them, else
//!    unsealed; each logged with the group id.
//!
//! [`transmit`] is `Command::Transmit` in a group: a message, an edit, a delete for everyone or
//! a reaction ([`super::outgoing`]) with the group context, then the
//! sync transcript of everyone this attempt reached (`isRecipientUpdate` on a follow-up to the
//! members an earlier attempt missed); per-recipient results merge into `TransmitOutcome::Partial`
//! when some failed. [`send_typing`] sends typing indicators the same way (online only).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, Envelope, OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_model::{
    BackendError, ConvKey, ErrorKind, LocalMsgId, RecipientState, SendPhase, UserId,
};
use libsignal_core::{Aci, DeviceId, ServiceId};
use libsignal_protocol::{
    CiphertextMessageType, ProtocolAddress, SessionRecord, SessionStore as _, SignalProtocolError,
    UnidentifiedSenderMessageContent, group_encrypt, sealed_sender_multi_recipient_encrypt,
};
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use super::content::{self, GroupContext, RecipientStatus, Thread, Transcript};
use super::sealed::{self, CertKind};
use super::sender_key::{self, Chain, MemberDevice};
use super::{
    Delivery, InGroup, Outgoing, SendContext, fetch_and_process, outgoing, send_error, send_to,
    usable_devices,
};
use crate::actor::Shared;
use crate::db::{self, Account};
use crate::error::{Error, Result};
use crate::groups::endorsements::{self, Endorsements};
use crate::groups::{self, GroupKeys, GroupRow, Membership};
use crate::msl;
use crate::net::{MultiRecipientOutcome, MultiRecipientSend, SignalTransport, TransportError};
use crate::padding::pad;
use crate::peers;
use crate::proto;
use crate::service_id::{SignalUser, aci_user_id, parse_user_id};
use crate::store::{
    IdentityKind, SqlSenderKeyStore, Stores, archive_all_sessions, archive_session, ready,
};

/// Most restarts of one group send (Desktop's `MAX_RECURSION`).
pub(crate) const MAX_RESTARTS: usize = 10;

/// Prekey fetches of one group send at once (Desktop's `fetchKeysForServiceIds` runs up to 50).
pub(crate) const PREKEY_FETCHES_AT_ONCE: usize = 32;

/// 1:1 sends of one group send at once: distribution messages and the 1:1 fan-out (Desktop
/// sends them with `Promise.all`; bounded here so a 1,000-member group does not open a
/// thousand requests). Sends to one member still wait for each other (`SendLocks`).
pub(crate) const MEMBER_SENDS_AT_ONCE: usize = 16;

/// Runs `f` for every item with at most `limit` of them in flight, on this task, and returns
/// the results in completion order.
async fn bounded<I, F, Fut>(items: I, limit: usize, f: F) -> Vec<Fut::Output>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: std::future::Future,
{
    use futures::StreamExt as _;
    futures::stream::iter(items)
        .map(f)
        .buffer_unordered(limit.max(1))
        .collect()
        .await
}

/// How often one phase of a group send is reported again with new counts: a few times a
/// second at most (a new phase, and the last count of one, always go out at once).
pub(crate) const PHASE_EVERY: Duration = Duration::from_millis(250);

/// Reports what a group send of a message is doing to the core
/// (`BackendEvent::TransmitProgress` with a phase, ephemeral; link feedback L6), throttled to
/// [`PHASE_EVERY`] per phase.
#[derive(Debug)]
pub(crate) struct Phases {
    events: mpsc::Sender<Envelope>,
    local: LocalMsgId,
    last: Cell<Option<(std::mem::Discriminant<SendPhase>, Instant)>>,
}

impl Phases {
    pub(crate) fn new(events: mpsc::Sender<Envelope>, local: LocalMsgId) -> Self {
        Self {
            events,
            local,
            last: Cell::new(None),
        }
    }

    /// Reports `phase` unless the same phase went out less than [`PHASE_EVERY`] ago. A lost
    /// report changes nothing (the core is gone, or the next one comes).
    pub(crate) async fn report(&self, phase: SendPhase) {
        let now = Instant::now();
        let kind = std::mem::discriminant(&phase);
        let complete = matches!(
            phase,
            SendPhase::SettingUpKeys { done, of } | SendPhase::Sending { done, of } if done >= of
        );
        if let Some((last, at)) = self.last.get()
            && last == kind
            && !complete
            && now.duration_since(at) < PHASE_EVERY
        {
            return;
        }
        self.last.set(Some((kind, now)));
        let _ = self
            .events
            .send(Envelope::ephemeral(BackendEvent::TransmitProgress {
                local: self.local,
                progress: 0.0,
                phase: Some(phase),
            }))
            .await;
    }
}

/// A member count for a phase.
fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The sender-key path needs at least this many recipients (Desktop step 5).
const MIN_SENDER_KEY_RECIPIENTS: usize = 2;

/// One `Content` for the members of a group.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupSend<'a> {
    /// The group.
    pub(crate) keys: &'a GroupKeys,
    /// What to send (unpadded).
    pub(crate) content: &'a proto::Content,
    /// The envelope timestamp.
    pub(crate) timestamp: u64,
    /// How.
    pub(crate) delivery: Delivery,
    /// The core's message id (progress and the Message Send Log).
    pub(crate) local: Option<&'a str>,
    /// Only these members (a resend); `None` = every member.
    pub(crate) only: Option<&'a [Aci]>,
    /// Where its phases go (a message's send; `None`: typing and other sends nobody watches).
    pub(crate) phases: Option<&'a Phases>,
}

impl GroupSend<'_> {
    async fn report(&self, phase: SendPhase) {
        if let Some(phases) = self.phases {
            phases.report(phase).await;
        }
    }
}

/// What a group send achieved, per recipient.
#[derive(Debug, Default)]
pub(crate) struct GroupOutcome {
    /// Recipients the server took it for, and whether sealed.
    pub(crate) delivered: Vec<(Aci, bool)>,
    /// Recipients it failed for.
    pub(crate) failed: Vec<(Aci, BackendError)>,
    /// How many recipients went the sender-key way (log and tests).
    pub(crate) sender_key_recipients: usize,
    /// Where the send's time went (the summary log line).
    pub(crate) parts: SendParts,
}

/// Counts and times of one group send's parts, for its summary log line (diagnosis §2 item 3):
/// tells a slow key setup from a refresh, device repairs or 1:1 sends.
#[derive(Debug, Default)]
pub(crate) struct SendParts {
    /// Passes of the restart loop after the first.
    pub(crate) restarts: usize,
    /// The group was refreshed for its endorsements, and how long that took.
    pub(crate) refreshed: bool,
    pub(crate) refresh: Duration,
    /// Members whose prekeys were fetched, and the time of those fetches.
    pub(crate) prekey_fetches: usize,
    pub(crate) fetch: Duration,
    /// Members that got our distribution message, and the time of those sends.
    pub(crate) skdm: usize,
    pub(crate) skdm_time: Duration,
    /// 409/410 entries repaired, and the time of the repairs.
    pub(crate) repairs: usize,
    pub(crate) repair: Duration,
    /// The multi-recipient send (encryption and request).
    pub(crate) multi: Duration,
    /// Members that got 1:1 sends, and the time of those.
    pub(crate) fanout: usize,
    pub(crate) fanout_time: Duration,
}

/// Whole milliseconds of `d` for a log field.
pub(crate) fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

impl GroupOutcome {
    fn handled(&self) -> BTreeSet<Aci> {
        self.delivered
            .iter()
            .map(|(a, _)| *a)
            .chain(self.failed.iter().map(|(a, _)| *a))
            .collect()
    }
}

/// A recipient of the sender-key path: its devices and their registration ids.
struct KeyRecipient {
    aci: Aci,
    devices: Vec<(DeviceId, u32)>,
}

impl KeyRecipient {
    fn member_devices(&self) -> impl Iterator<Item = MemberDevice> + '_ {
        let recipient = ServiceId::from(self.aci).service_id_string();
        self.devices
            .iter()
            .map(move |(device, registration_id)| MemberDevice {
                recipient: recipient.clone(),
                device: u32::from(*device),
                registration_id: *registration_id,
            })
    }
}

/// Log-safe form of a member.
fn log_safe(aci: Aci) -> SignalUser {
    SignalUser::Aci(aci)
}

/// The recipients of a send in group `row` (step 1, `getMessageRecipients`).
fn recipients(
    conn: &kourier_db::rusqlite::Connection,
    row: &GroupRow,
    account: &Account,
    only: Option<&[Aci]>,
) -> Result<Vec<Aci>> {
    let Some(snapshot) = row.snapshot.as_ref() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(snapshot.members.len());
    for member in &snapshot.members {
        let aci = Aci::from(member.aci);
        if aci == account.aci || only.is_some_and(|only| !only.contains(&aci)) {
            continue;
        }
        let peer = peers::load(conn, &ServiceId::from(aci).service_id_string())?;
        if peer.blocked || peer.unregistered {
            continue;
        }
        out.push(aci);
    }
    Ok(out)
}

/// The devices of a member the sender-key path can encrypt to.
enum MemberDevices {
    /// Device ids and registration ids (empty: no usable session).
    Usable(Vec<(DeviceId, u32)>),
    /// A session's identity is not trusted for sending (a verified contact's safety number
    /// changed and the user has not confirmed it). `sealed_sender_multi_recipient_encrypt`
    /// does not check trust, so the member goes the 1:1 way, where `message_encrypt` refuses
    /// with `UntrustedIdentity` as for a 1:1 chat.
    Untrusted,
}

/// The devices and registration ids of `aci` we can encrypt to (strict usability unless a
/// prekey fetch just built the sessions).
fn devices_of(
    conn: &kourier_db::rusqlite::Connection,
    account: &Account,
    aci: Aci,
    strict: bool,
    now: i64,
) -> Result<MemberDevices> {
    use libsignal_protocol::{Direction, IdentityKey, IdentityKeyStore as _};
    let their = ServiceId::from(aci).service_id_string();
    let devices = usable_devices(conn, &their, None, strict)?;
    let stores = Stores::new(
        conn,
        IdentityKind::Aci,
        account.aci_identity,
        account.aci_registration_id,
        now,
    );
    let mut out = Vec::with_capacity(devices.len());
    for device in devices {
        let address = ProtocolAddress::new(their.clone(), device);
        let Some(session) = ready(stores.session.load_session(&address))?? else {
            continue;
        };
        let trusted = match session.remote_identity_key_bytes()? {
            Some(bytes) => ready(stores.identity.is_trusted_identity(
                &address,
                &IdentityKey::decode(&bytes)?,
                Direction::Sending,
            ))??,
            None => false,
        };
        if !trusted {
            return Ok(MemberDevices::Untrusted);
        }
        out.push((device, session.remote_registration_id()?));
    }
    Ok(MemberDevices::Usable(out))
}

/// Sends `send` to the members of its group; see the module docs.
pub(crate) async fn send_to_group<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    account: &Account,
    send: &GroupSend<'_>,
) -> Result<GroupOutcome> {
    let ctx = shared.send_context(account, socket);
    let keys = send.keys;
    let group_id = &keys.id[..];
    let _serialized = ctx
        .locks
        .lock(&format!("group:{}", groups::job_key(group_id)))
        .await;
    let bytes = send.content.encode_to_vec();
    let mut outcome = GroupOutcome::default();
    // Members whose sessions a prekey fetch just built; members that must get 1:1 sends.
    let mut fetched: BTreeSet<Aci> = BTreeSet::new();
    let mut one_to_one: BTreeSet<Aci> = BTreeSet::new();
    let mut refreshed = false;
    let mut endorsed: Option<Endorsements> = None;
    let mut endorsed_stamp: Option<(Option<u32>, Option<i64>, bool)> = None;
    let mut fan_out: Option<Vec<Aci>> = None;
    for pass in 0..MAX_RESTARTS {
        outcome.parts.restarts = pass;
        let now = ctx.now();
        let Some(row) = ctx.db.read(|c| groups::load_by_id(c, group_id))? else {
            return Err(Error::Internal("the group vanished during a send"));
        };
        if row.membership != Membership::Member {
            // A refresh (ours, or a group change applied meanwhile) says we left or were
            // removed: the members would drop the message anyway.
            return Err(Error::Internal("no longer a member of the group"));
        }
        let handled = outcome.handled();
        let mut members = ctx.db.read(|c| recipients(c, &row, account, send.only))?;
        members.retain(|m| !handled.contains(m));
        // Members the server has this message for already (a repeated Transmit).
        if let Some(local) = send.local {
            let mut left = Vec::with_capacity(members.len());
            for member in members {
                let sid = ServiceId::from(member).service_id_string();
                match ctx.db.read(|c| msl::accepted(c, local, &sid))? {
                    Some(sealed) => outcome.delivered.push((member, sealed)),
                    None => left.push(member),
                }
            }
            members = left;
        }
        if members.is_empty() {
            fan_out = Some(Vec::new());
            break;
        }
        // Members of this send: the ones it is done for and the ones left.
        let of = count(outcome.delivered.len() + outcome.failed.len() + members.len());

        // Step 2: endorsements, refreshing the group once when they are unusable. Verifying
        // them costs a zk check per member, so a restart reuses them until the group changed.
        let stamp = (row.revision, row.endorsements_expire_at, refreshed);
        if endorsed_stamp != Some(stamp) {
            endorsed = match shared.zk_params.as_ref() {
                Some(server) => ctx.db.read(|c| endorsements::load(c, &row, server, now))?,
                None => None,
            };
            endorsed_stamp = Some(stamp);
        }
        if endorsed.is_none() && !refreshed && shared.zk_params.is_some() {
            refreshed = true;
            outcome.parts.refreshed = true;
            send.report(SendPhase::RefreshingGroup).await;
            let started = Instant::now();
            let refresh = groups::refresh(shared, socket, account, &row).await;
            outcome.parts.refresh += started.elapsed();
            match refresh {
                Ok(()) => {
                    tracing::debug!(
                        group = %groups::log_id(group_id),
                        "group refreshed for its send endorsements"
                    );
                    continue;
                }
                Err(e) => tracing::warn!(
                    group = %groups::log_id(group_id),
                    error = %e,
                    "group refresh before a send failed; members get 1:1 sends"
                ),
            }
        }
        let Some(tokens) = endorsed.as_ref() else {
            fan_out = Some(members);
            break;
        };

        // Step 3: our chain.
        let max_age = ctx.db.read(sender_key::max_age)?;
        let chain = ctx.db.zone(|tx| -> Result<Chain> {
            let chain = sender_key::ensure(tx, group_id, now)?;
            if sender_key::expired(&chain, max_age, now) {
                sender_key::reset(tx, account, group_id, now)?;
                return sender_key::chain(tx, group_id)?
                    .ok_or(Error::Internal("sender key info vanished"));
            }
            Ok(chain)
        })?;

        // Step 4: sessions for members without a usable one.
        let mut to_fetch = Vec::new();
        let mut key_recipients = Vec::new();
        let mut rest = Vec::new();
        for member in &members {
            if one_to_one.contains(member) || !tokens.has(*member) {
                rest.push(*member);
                continue;
            }
            let strict = !fetched.contains(member);
            let devices = match ctx
                .db
                .read(|c| devices_of(c, account, *member, strict, now))?
            {
                MemberDevices::Usable(devices) => devices,
                MemberDevices::Untrusted => {
                    rest.push(*member);
                    continue;
                }
            };
            if devices.is_empty() {
                if fetched.contains(member) {
                    rest.push(*member);
                } else {
                    to_fetch.push(*member);
                }
            } else {
                key_recipients.push(KeyRecipient {
                    aci: *member,
                    devices,
                });
            }
        }
        if !to_fetch.is_empty() {
            fetched.extend(to_fetch.iter().copied());
            outcome.parts.prekey_fetches += to_fetch.len();
            let started = Instant::now();
            let keys_of = count(to_fetch.len());
            send.report(SendPhase::SettingUpKeys {
                done: 0,
                of: keys_of,
            })
            .await;
            // Once, so the parallel fetches below find it cached instead of each asking.
            sealed::warm_certificate(&ctx).await?;
            let ctx = &ctx;
            let done = Cell::new(0_u32);
            let done = &done;
            let failed = bounded(to_fetch, PREKEY_FETCHES_AT_ONCE, |member| {
                let token = tokens.token(&[member]);
                async move {
                    let fetched = fetch_for(ctx, member, token.as_deref()).await;
                    done.set(done.get() + 1);
                    send.report(SendPhase::SettingUpKeys {
                        done: done.get(),
                        of: keys_of,
                    })
                    .await;
                    let e = fetched.err()?;
                    // The 1:1 send repeats the fetch and reports the error for this member.
                    tracing::debug!(member = ?log_safe(member), error = %e, "prekey fetch failed");
                    Some(member)
                }
            })
            .await;
            outcome.parts.fetch += started.elapsed();
            one_to_one.extend(failed.into_iter().flatten());
            continue;
        }

        // Step 5.
        if key_recipients.len() < MIN_SENDER_KEY_RECIPIENTS {
            fan_out = Some(members);
            break;
        }

        // Step 6: who has our chain. A resend to some members, or a repeated Transmit that
        // skips the members the server already took it for, is partial: the members it
        // leaves out did not leave and keep our chain.
        let current: BTreeSet<MemberDevice> = key_recipients
            .iter()
            .flat_map(KeyRecipient::member_devices)
            .collect();
        let known = ctx.db.read(|c| sender_key::devices(c, group_id))?;
        let partial = send.only.is_some() || !outcome.delivered.is_empty();
        let diff = sender_key::diff(&known, &current, partial);
        let snapshot_members: BTreeSet<String> = row
            .snapshot
            .as_ref()
            .map(|s| {
                s.members
                    .iter()
                    .map(|m| ServiceId::from(Aci::from(m.aci)).service_id_string())
                    .collect()
            })
            .unwrap_or_default();
        if diff
            .removed
            .iter()
            .any(|d| !snapshot_members.contains(&d.recipient))
        {
            tracing::info!(
                group = %groups::log_id(group_id),
                "a member with our sender key left; starting a new chain"
            );
            ctx.db
                .zone(|tx| sender_key::reset(tx, account, group_id, now))?;
            continue;
        }
        if !diff.new.is_empty() {
            // The distribution messages are sealed: the certificate is fetched once here when
            // no earlier step did (every member may have a session already), not by every one
            // of the sends below at the same time.
            sealed::warm_certificate(&ctx).await?;
            outcome.parts.skdm += diff
                .new
                .iter()
                .map(|d| d.recipient.as_str())
                .collect::<BTreeSet<_>>()
                .len();
            let started = Instant::now();
            let failed = distribute(shared, &ctx, account, send, &chain, &diff.new, tokens).await;
            outcome.parts.skdm_time += started.elapsed();
            failed?.into_iter().for_each(|failed| {
                one_to_one.insert(failed);
            });
            continue;
        }
        if !diff.removed.is_empty() {
            ctx.db
                .zone(|tx| sender_key::remove_devices(tx, group_id, &diff.removed))?;
        }

        // Step 7: encrypt and send.
        send.report(SendPhase::Sending {
            done: count(outcome.delivered.len()),
            of,
        })
        .await;
        let acis: Vec<Aci> = key_recipients.iter().map(|r| r.aci).collect();
        let Some(token) = tokens.token(&acis) else {
            fan_out = Some(members);
            break;
        };
        let certificate = match sealed::certificate(&ctx, CertKind::WithoutE164).await {
            Ok(certificate) => certificate,
            Err(e @ Error::Db(_)) => return Err(e),
            Err(e) => {
                tracing::warn!(error = %e, "no sender certificate; members get 1:1 sends");
                fan_out = Some(members);
                break;
            }
        };
        let multi_started = Instant::now();
        let padded = pad(&bytes);
        let encrypted = ctx.db.zone(|tx| {
            encrypt_multi(
                tx,
                account,
                &chain,
                &padded,
                certificate.clone(),
                send.delivery,
                group_id,
                &key_recipients,
                now,
            )
        });
        let payload = match encrypted {
            Ok(payload) => payload,
            Err(Error::Protocol(SignalProtocolError::InvalidRegistrationId(address, _))) => {
                // `InvalidRegistrationId`: this account cannot take sealed sender v2.
                if let Some(ServiceId::Aci(aci)) =
                    ServiceId::parse_from_service_id_string(address.name())
                {
                    one_to_one.insert(aci);
                    let sid = address.name().to_owned();
                    ctx.db.zone(|tx| {
                        peers::set_sealed_state(tx, &sid, peers::SealedSenderState::Disabled, now)
                    })?;
                    continue;
                }
                return Err(Error::Internal("invalid registration id of a non-member"));
            }
            Err(Error::Protocol(SignalProtocolError::NoSenderKeyState { .. })) => {
                // Our chain is gone (the device list said otherwise): start over with a new one.
                ctx.db
                    .zone(|tx| sender_key::reset(tx, account, group_id, now))?;
                continue;
            }
            Err(e) => return Err(e),
        };
        kourier_db::failpoint::hit(super::FAILPOINT_AFTER_ENCRYPT_COMMIT);
        if let Some(point) = send.delivery.failpoint {
            kourier_db::failpoint::hit(point);
        }
        let request = MultiRecipientSend {
            payload: payload.into(),
            timestamp: send.timestamp,
            group_send_token: Zeroizing::new(token),
            online: send.delivery.online,
            urgent: send.delivery.urgent,
        };
        let unauth = ctx.unauth.get(ctx.net, ctx.now_ms()).await;
        let answer = match unauth {
            Ok(unauth_socket) => {
                let answer = ctx.net.send_multi_recipient(&unauth_socket, request).await;
                if answer.is_err() {
                    ctx.unauth.invalidate(&unauth_socket);
                }
                answer
            }
            Err(e) => Err(e),
        };
        outcome.parts.multi += multi_started.elapsed();
        match answer {
            Ok(MultiRecipientOutcome::Sent { unregistered }) => {
                let now = ctx.now();
                let unregistered: BTreeSet<ServiceId> = unregistered.into_iter().collect();
                ctx.db.zone(|tx| -> Result<()> {
                    for recipient in &key_recipients {
                        let service_id = ServiceId::from(recipient.aci);
                        let sid = service_id.service_id_string();
                        if unregistered.contains(&service_id) {
                            peers::mark_unregistered(tx, &sid, now)?;
                            archive_all_sessions(tx, &sid, now)?;
                            continue;
                        }
                        if let Some(local) = send.local {
                            msl::record_accepted(tx, local, &sid, true, now)?;
                        }
                        if send.delivery.log {
                            let devices: Vec<DeviceId> =
                                recipient.devices.iter().map(|(d, _)| *d).collect();
                            msl::record(
                                tx,
                                &msl::Payload {
                                    timestamp: send.timestamp,
                                    content: &bytes,
                                    hint: send.delivery.hint,
                                    urgent: send.delivery.urgent,
                                    has_pni_signature: false,
                                    local: send.local,
                                    group_id: Some(group_id),
                                },
                                &sid,
                                &devices,
                                now,
                            )?;
                        }
                    }
                    Ok(())
                })?;
                for recipient in &key_recipients {
                    if unregistered.contains(&ServiceId::from(recipient.aci)) {
                        outcome.failed.push((
                            recipient.aci,
                            BackendError::new(ErrorKind::PeerUnregistered, "not on Signal"),
                        ));
                    } else {
                        outcome.delivered.push((recipient.aci, true));
                    }
                }
                outcome.sender_key_recipients = key_recipients.len();
                send.report(SendPhase::Sending {
                    done: count(outcome.delivered.len()),
                    of,
                })
                .await;
                tracing::debug!(
                    group = %groups::log_id(group_id),
                    recipients = key_recipients.len(),
                    "sender-key message accepted by the server"
                );
                fan_out = Some(rest);
                break;
            }
            Ok(MultiRecipientOutcome::Unauthorized) => {
                tracing::info!(
                    group = %groups::log_id(group_id),
                    "the group send token was refused; members get 1:1 sends"
                );
                fan_out = Some(members);
                break;
            }
            Ok(MultiRecipientOutcome::MismatchedDevices(entries)) => {
                send.report(SendPhase::RepairingDevices).await;
                let started = Instant::now();
                outcome.parts.repairs += entries.len();
                for entry in entries {
                    repair(&ctx, &entry, tokens).await?;
                }
                outcome.parts.repair += started.elapsed();
                tracing::debug!(group = %groups::log_id(group_id), "device lists repaired");
                continue;
            }
            Err(TransportError::InvalidResponse(reason)) => {
                // An answer the typed API has no case for (the REST route's 404 for an unknown
                // recipient, say): Desktop fails over to 1:1 sends for every error it does not
                // fail on (`_shouldFailSend`); connectivity, rate limits and server errors
                // still fail below.
                tracing::warn!(
                    group = %groups::log_id(group_id),
                    reason = %reason,
                    "unexpected answer to the sender-key send; members get 1:1 sends"
                );
                fan_out = Some(members);
                break;
            }
            Err(e) => {
                let e = Error::from(e);
                for recipient in &key_recipients {
                    let user = aci_user_id(recipient.aci)
                        .map_err(|_| Error::Internal("member id does not form a user id"))?;
                    outcome.failed.push((recipient.aci, send_error(&e, &user)));
                }
                tracing::warn!(
                    group = %groups::log_id(group_id),
                    error = %e,
                    "sender-key send failed"
                );
                fan_out = Some(rest);
                break;
            }
        }
    }
    let fan_out = match fan_out {
        Some(fan_out) => fan_out,
        None => {
            tracing::warn!(
                group = %groups::log_id(group_id),
                "group send state kept changing; members get 1:1 sends"
            );
            let handled = outcome.handled();
            let row = ctx.db.read(|c| groups::load_by_id(c, group_id))?;
            match row {
                Some(row) => ctx
                    .db
                    .read(|c| recipients(c, &row, account, send.only))?
                    .into_iter()
                    .filter(|m| !handled.contains(m))
                    .collect(),
                None => Vec::new(),
            }
        }
    };
    // Step 9: 1:1 sends, several members at once (Desktop's `sendToGroup` legacy path sends
    // to every member with `Promise.all`).
    let ctx_ref = &ctx;
    let endorsed = endorsed.as_ref();
    outcome.parts.fanout = fan_out.len();
    let fanout_started = Instant::now();
    let of = count(outcome.delivered.len() + outcome.failed.len() + fan_out.len());
    let done = Cell::new(count(outcome.delivered.len()));
    let done = &done;
    if !fan_out.is_empty() {
        send.report(SendPhase::Sending {
            done: done.get(),
            of,
        })
        .await;
    }
    if !fan_out.is_empty() {
        // The same for the 1:1 sends: one fetch of the certificate, not one per member.
        sealed::warm_certificate(ctx_ref).await?;
    }
    let sent = bounded(fan_out, MEMBER_SENDS_AT_ONCE, |member| async move {
        let token = endorsed.and_then(|e| e.token(&[member]));
        let out = Outgoing {
            content: send.content,
            timestamp: send.timestamp,
            delivery: send.delivery,
            local: send.local,
            group: Some(InGroup {
                id: group_id,
                token: token.as_deref(),
                named: true,
            }),
        };
        let result = send_to(ctx_ref, ServiceId::from(member), &out).await;
        if result.is_ok() {
            done.set(done.get() + 1);
            send.report(SendPhase::Sending {
                done: done.get(),
                of,
            })
            .await;
        }
        (member, result)
    })
    .await;
    outcome.parts.fanout_time = fanout_started.elapsed();
    for (member, result) in sent {
        match result {
            Ok(accepted) => outcome.delivered.push((member, accepted.sealed)),
            Err(e) => {
                let user = aci_user_id(member)
                    .map_err(|_| Error::Internal("member id does not form a user id"))?;
                tracing::debug!(member = ?log_safe(member), error = %e, "1:1 group send failed");
                outcome.failed.push((member, send_error(&e, &user)));
            }
        }
    }
    Ok(outcome)
}

/// A prekey fetch for `member` (sealed with the access key or the endorsement token).
async fn fetch_for<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    member: Aci,
    token: Option<&[u8]>,
) -> Result<()> {
    let destination = ServiceId::from(member);
    let mut seal = sealed::seal_for(ctx, destination, token).await?;
    fetch_and_process(ctx, destination, None, None, &mut seal).await
}

/// Step 6: our distribution message to the members with `new` devices (1:1, sealed, not online
/// only, with the content hint and urgency of the send it prepares: Desktop's `sendToGroup`
/// passes its `contentHint` on to `sendSenderKeyDistributionMessage`, so a typing send's
/// distribution message is implicit and its loss never shows as a delivery issue), recorded per
/// member once the server took it. Returns the members it failed for.
async fn distribute<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    ctx: &SendContext<'_, T>,
    account: &Account,
    send: &GroupSend<'_>,
    chain: &Chain,
    new: &[MemberDevice],
    tokens: &Endorsements,
) -> Result<Vec<Aci>> {
    let group_id = &send.keys.id[..];
    let now = ctx.now();
    let message = ctx
        .db
        .zone(|tx| sender_key::distribution_message(tx, account, chain, now))?;
    let distribution = content::distribution_content(message);
    let delivery = Delivery {
        online: false,
        log: true,
        may_seal: true,
        ..send.delivery
    };
    let mut by_member: BTreeMap<&str, Vec<&MemberDevice>> = BTreeMap::new();
    for device in new {
        by_member.entry(&device.recipient).or_default().push(device);
    }
    let to_send: Vec<(Aci, Vec<&MemberDevice>)> = by_member
        .into_iter()
        .filter_map(|(recipient, devices)| {
            match ServiceId::parse_from_service_id_string(recipient) {
                Some(ServiceId::Aci(aci)) => Some((aci, devices)),
                _ => None,
            }
        })
        .collect();
    let distribution = &distribution;
    let keys_of = count(to_send.len());
    send.report(SendPhase::SettingUpKeys {
        done: 0,
        of: keys_of,
    })
    .await;
    let done = Cell::new(0_u32);
    let done = &done;
    let sent = bounded(to_send, MEMBER_SENDS_AT_ONCE, |(aci, devices)| {
        let token = tokens.token(&[aci]);
        let timestamp = shared.unique_timestamp();
        async move {
            let out = Outgoing {
                content: distribution,
                timestamp,
                delivery,
                local: None,
                group: Some(InGroup {
                    id: group_id,
                    token: token.as_deref(),
                    named: true,
                }),
            };
            let result = send_to(ctx, ServiceId::from(aci), &out).await;
            if result.is_ok() {
                // Recorded as soon as the server took it, so a failure elsewhere keeps it.
                ctx.db
                    .zone(|tx| sender_key::add_devices(tx, group_id, devices.iter().copied()))?;
            }
            done.set(done.get() + 1);
            send.report(SendPhase::SettingUpKeys {
                done: done.get(),
                of: keys_of,
            })
            .await;
            Ok::<_, Error>((aci, result))
        }
    })
    .await;
    let mut failed = Vec::new();
    for sent in sent {
        let (aci, result) = sent?;
        if let Err(e) = result {
            tracing::info!(
                member = ?log_safe(aci),
                error = %e,
                "sender key distribution failed; the member gets 1:1 sends"
            );
            failed.push(aci);
        }
    }
    tracing::debug!(
        group = %groups::log_id(group_id),
        devices = new.len(),
        "sender key distributed"
    );
    Ok(failed)
}

/// Step 8, 409/410: archive extra and stale sessions, forget stale devices' copies of our chain,
/// build sessions for missing and stale devices (`handleMismatchedDevicesError`).
async fn repair<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    entry: &crate::net::DeviceMismatch,
    tokens: &Endorsements,
) -> Result<()> {
    let their = entry.account.service_id_string();
    let now = ctx.now();
    ctx.db.zone(|tx| -> Result<()> {
        for device in entry.extra.iter().chain(&entry.stale) {
            archive_session(tx, IdentityKind::Aci, &their, *device, now)?;
        }
        for device in &entry.stale {
            sender_key::forget_device(tx, &their, u32::from(*device))?;
        }
        Ok(())
    })?;
    let token = match entry.account {
        ServiceId::Aci(aci) => tokens.token(&[aci]),
        ServiceId::Pni(_) => None,
    };
    let mut seal = sealed::seal_for(ctx, entry.account, token.as_deref()).await?;
    for device in entry.missing.iter().chain(&entry.stale) {
        fetch_and_process(ctx, entry.account, Some(*device), None, &mut seal).await?;
    }
    Ok(())
}

/// Step 7 inside one zone: the sender-key message, sealed for every device of every recipient.
#[allow(
    clippy::too_many_arguments,
    reason = "one zone body, all inputs are distinct"
)]
fn encrypt_multi(
    tx: &kourier_db::Tx<'_>,
    account: &Account,
    chain: &Chain,
    padded: &[u8],
    certificate: libsignal_protocol::SenderCertificate,
    delivery: Delivery,
    group_id: &[u8],
    recipients: &[KeyRecipient],
    now: i64,
) -> Result<Vec<u8>> {
    let mut sender_keys = SqlSenderKeyStore::new(tx, now);
    let message = ready(group_encrypt(
        &mut sender_keys,
        &account.address(IdentityKind::Aci),
        chain.distribution_id,
        padded,
        &mut rand::rng(),
    ))??;
    let usmc = UnidentifiedSenderMessageContent::new(
        CiphertextMessageType::SenderKey,
        certificate,
        message.serialized().to_vec(),
        delivery.hint,
        Some(group_id.to_vec()),
    )?;
    let stores = Stores::new(
        tx,
        IdentityKind::Aci,
        account.aci_identity,
        account.aci_registration_id,
        now,
    );
    let mut addresses = Vec::new();
    let mut sessions: Vec<SessionRecord> = Vec::new();
    for recipient in recipients {
        let their = ServiceId::from(recipient.aci).service_id_string();
        for (device, _) in &recipient.devices {
            let address = ProtocolAddress::new(their.clone(), *device);
            let session = ready(stores.session.load_session(&address))??
                .ok_or(Error::Internal("session vanished before a group send"))?;
            addresses.push(address);
            sessions.push(session);
        }
    }
    let address_refs: Vec<&ProtocolAddress> = addresses.iter().collect();
    let session_refs: Vec<&SessionRecord> = sessions.iter().collect();
    Ok(ready(sealed_sender_multi_recipient_encrypt(
        &address_refs,
        &session_refs,
        Vec::<ServiceId>::new(),
        &usmc,
        &stores.identity,
        &mut rand::rng(),
    ))??)
}

/// Handles `Command::Transmit` in a group (module docs); `uploader` prepares the body once the
/// group is known to take it.
pub(crate) async fn transmit<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    account: &Account,
    req: &OutgoingRequest,
    uploader: &super::media::Uploader<'_, T>,
) -> TransmitOutcome {
    match transmit_message(shared, socket, account, req, uploader).await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(kind = error.kind.name(), error = %error.detail, "group transmit failed");
            TransmitOutcome::Failed(error)
        }
    }
}

/// The group of a conversation key, if we may write to it.
fn writable_group(
    conn: &kourier_db::rusqlite::Connection,
    conv: &ConvKey,
    account: &Account,
) -> std::result::Result<GroupRow, BackendError> {
    let id = groups::id_of_conv(conv)
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "not a Signal group key"))?;
    let row = groups::load_by_id(conn, &id)
        .map_err(|e| e.to_backend_error())?
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "unknown group"))?;
    let snapshot = row.snapshot.as_ref();
    if row.membership != Membership::Member || snapshot.is_none_or(|s| s.terminated) {
        return Err(
            BackendError::new(ErrorKind::Unsupported, "not a member of the group")
                .with_user_message("You are not a member of this group."),
        );
    }
    let admin = snapshot
        .and_then(|s| s.member(account.aci))
        .is_some_and(|m| m.admin);
    if snapshot.is_some_and(|s| s.announcements_only) && !admin {
        return Err(
            BackendError::new(ErrorKind::Unsupported, "only admins may send")
                .with_user_message("Only admins can send messages to this group."),
        );
    }
    if row.blocked {
        return Err(BackendError::new(
            ErrorKind::Unsupported,
            "the group is blocked",
        ));
    }
    Ok(row)
}

async fn transmit_message<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    account: &Account,
    req: &OutgoingRequest,
    uploader: &super::media::Uploader<'_, T>,
) -> std::result::Result<TransmitOutcome, BackendError> {
    let started = Instant::now();
    let timestamp = u64::try_from(req.sent_at)
        .map_err(|_| BackendError::new(ErrorKind::InvalidInput, "negative sent_at"))?;
    if let OutgoingKind::Delete {
        target,
        for_everyone: false,
    } = &req.kind
    {
        // Deleting for ourselves needs no membership: only our own devices hear of it.
        let id = groups::id_of_conv(&req.conv)
            .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "not a Signal group key"))?;
        let ctx = shared.send_context(account, socket);
        return outgoing::sync_delete_for_me(&ctx, &Thread::Group(id.to_vec()), target, req).await;
    }
    let row = shared
        .db
        .read(|c| Ok::<_, Error>(writable_group(c, &req.conv, account)))
        .map_err(|e| e.to_backend_error())??;
    let snapshot = row
        .snapshot
        .as_ref()
        .ok_or_else(|| BackendError::new(ErrorKind::Internal, "group without state"))?;
    let chat = outgoing::Chat {
        account,
        timer: snapshot.timer_secs,
        timer_version: None,
        group: Some(GroupContext {
            master_key: &row.keys.master,
            revision: snapshot.revision,
        }),
    };
    let built = outgoing::build(&shared.db, uploader, &chat, req, shared.now()).await?;
    let outgoing = built.wire.content();
    let only: Option<Vec<Aci>> = req.only.as_ref().map(|only| {
        only.iter()
            .filter_map(|user| match parse_user_id(user) {
                Ok(SignalUser::Aci(aci)) => Some(aci),
                _ => None,
            })
            .collect()
    });
    let local = &*req.local;
    let built_in = started.elapsed();
    let phases = Phases::new(shared.events.clone(), uploader.local);
    let outcome = send_to_group(
        shared,
        socket,
        account,
        &GroupSend {
            keys: &row.keys,
            content: &outgoing,
            timestamp,
            delivery: built.delivery,
            local: Some(local),
            only: only.as_deref(),
            phases: Some(&phases),
        },
    )
    .await
    .map_err(|e| e.to_backend_error())?;
    let mut transcript_time = Duration::ZERO;
    // A lonely group (nobody else to send to) still syncs the message, like Note to Self; a
    // follow-up whose members all left has nothing to tell.
    let lonely = req.only.is_none() && outcome.delivered.is_empty() && outcome.failed.is_empty();
    if !outcome.delivered.is_empty() || lonely {
        let statuses: Vec<RecipientStatus> = outcome
            .delivered
            .iter()
            .map(|(aci, sealed)| RecipientStatus {
                recipient: ServiceId::from(*aci),
                sealed: *sealed,
            })
            .collect();
        let transcript = content::sent_transcript(&Transcript {
            sent: &built.wire,
            timestamp,
            destination: None,
            statuses: &statuses,
            expiration_start: built.expires.then(|| shared.now_ms()),
            is_recipient_update: req.is_recipient_update,
        });
        // A follow-up's transcript is a message of its own: its progress key names the members.
        let key = if req.is_recipient_update {
            update_key(local, &outcome.delivered)
        } else {
            local.to_owned()
        };
        let ctx = shared.send_context(account, socket);
        phases.report(SendPhase::SyncingOwnDevices).await;
        let transcript_started = Instant::now();
        let synced = super::sync_transcript(&ctx, &transcript, timestamp, &key, lonely).await;
        transcript_time = transcript_started.elapsed();
        if let Err(e) = synced {
            log_group_send(&row, req, &outcome, built_in, transcript_time, started);
            return Err(e.to_backend_error());
        }
    }
    log_group_send(&row, req, &outcome, built_in, transcript_time, started);
    Ok(merge(outcome))
}

/// The one summary line of a group send (diagnosis §2 item 3): counts and milliseconds only.
fn log_group_send(
    row: &GroupRow,
    req: &OutgoingRequest,
    outcome: &GroupOutcome,
    built: Duration,
    transcript: Duration,
    started: Instant,
) {
    let parts = &outcome.parts;
    tracing::info!(
        group = %groups::log_id(&row.keys.id),
        request = req.kind.name(),
        members = outcome.delivered.len() + outcome.failed.len(),
        delivered = outcome.delivered.len(),
        failed = outcome.failed.len(),
        sender_key = outcome.sender_key_recipients,
        restarts = parts.restarts,
        refreshed = parts.refreshed,
        refresh_ms = millis(parts.refresh),
        prekey_fetches = parts.prekey_fetches,
        fetch_ms = millis(parts.fetch),
        skdm = parts.skdm,
        skdm_ms = millis(parts.skdm_time),
        repairs = parts.repairs,
        repair_ms = millis(parts.repair),
        multi_ms = millis(parts.multi),
        fanout = parts.fanout,
        fanout_ms = millis(parts.fanout_time),
        build_ms = millis(built),
        transcript_ms = millis(transcript),
        total_ms = millis(started.elapsed()),
        "group message sent"
    );
}

/// The progress key of a recipient-update transcript (`<local>:update:<digest of members>`).
fn update_key(local: &str, delivered: &[(Aci, bool)]) -> String {
    let mut digest = Sha256::new();
    let mut members: Vec<String> = delivered
        .iter()
        .map(|(aci, _)| ServiceId::from(*aci).service_id_string())
        .collect();
    members.sort();
    for member in &members {
        digest.update(member.as_bytes());
        digest.update([0]);
    }
    let digest = digest.finalize();
    let mut short = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(short, "{byte:02x}");
    }
    format!("{local}:update:{short}")
}

/// How bad a failure is for the whole attempt (lower = reported first).
fn severity(kind: &ErrorKind) -> u8 {
    match kind {
        ErrorKind::Challenge { .. } => 0,
        ErrorKind::RateLimited { .. } => 1,
        ErrorKind::Transient { .. } => 2,
        ErrorKind::UntrustedIdentity { .. } => 3,
        _ => 4,
    }
}

/// The per-recipient results as the core takes them (§4.8 step 3): everyone reached → `Sent`;
/// some reached → `Partial`; nobody → `Failed` with the most pressing error (the core retries
/// the whole message; an `isRecipientUpdate` follow-up needs a first transcript to update).
pub(crate) fn merge(outcome: GroupOutcome) -> TransmitOutcome {
    let delivered: Vec<(UserId, RecipientState)> = outcome
        .delivered
        .iter()
        .filter_map(|(aci, sealed)| {
            Some((
                aci_user_id(*aci).ok()?,
                RecipientState::Sent { sealed: *sealed },
            ))
        })
        .collect();
    let mut failed: Vec<(UserId, BackendError)> = outcome
        .failed
        .into_iter()
        .filter_map(|(aci, error)| Some((aci_user_id(aci).ok()?, error)))
        .collect();
    if failed.is_empty() {
        return TransmitOutcome::Sent {
            server_at: None,
            per_recipient: delivered,
        };
    }
    if delivered.is_empty() {
        failed.sort_by_key(|(_, e)| severity(&e.kind));
        let (_, first) = failed.swap_remove(0);
        return TransmitOutcome::Failed(first);
    }
    TransmitOutcome::Partial { delivered, failed }
}

/// `Command::SetTyping` in a group: a `TypingMessage` with the group id through the group send
/// (sender key where possible, online only, never logged), when the typing setting is on.
pub(crate) async fn send_typing<T: SignalTransport>(
    shared: Rc<Shared<T>>,
    conv: ConvKey,
    on: bool,
) {
    let (Some(account), Some(socket)) = (shared.account(), shared.socket.borrow().clone()) else {
        return;
    };
    let row = shared.db.read(|c| -> Result<Option<GroupRow>> {
        if !db::synced_settings(c)?.typing_indicators {
            return Ok(None);
        }
        Ok(writable_group(c, &conv, &account).ok())
    });
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = %e, "group typing: reading settings failed");
            return;
        }
    };
    let timestamp = shared.unique_timestamp();
    let typing = content::typing_in(on, timestamp, Some(&row.keys.id));
    let sent = send_to_group(
        &shared,
        &socket,
        &account,
        &GroupSend {
            keys: &row.keys,
            content: &typing,
            timestamp,
            delivery: Delivery::TYPING,
            local: None,
            only: None,
            phases: None,
        },
    )
    .await;
    match sent {
        Ok(outcome) if !outcome.failed.is_empty() => tracing::debug!(
            failed = outcome.failed.len(),
            "group typing indicator not sent to every member"
        ),
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "group typing indicator not sent"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aci(n: u128) -> Aci {
        Aci::from(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn results_merge_per_recipient() {
        let all = merge(GroupOutcome {
            delivered: vec![(aci(1), true), (aci(2), false)],
            ..GroupOutcome::default()
        });
        assert!(
            matches!(&all, TransmitOutcome::Sent { per_recipient, .. } if per_recipient.len() == 2),
            "{all:?}"
        );
        let partial = merge(GroupOutcome {
            delivered: vec![(aci(1), true)],
            failed: vec![(
                aci(2),
                BackendError::new(ErrorKind::RateLimited { retry_after: None }, "429"),
            )],
            ..GroupOutcome::default()
        });
        match partial {
            TransmitOutcome::Partial { delivered, failed } => {
                assert_eq!(delivered.len(), 1);
                assert_eq!(failed.len(), 1);
                assert!(failed[0].1.is_retryable());
            }
            other => panic!("{other:?}"),
        }
        let none = merge(GroupOutcome {
            failed: vec![
                (
                    aci(1),
                    BackendError::new(ErrorKind::PeerUnregistered, "404"),
                ),
                (
                    aci(2),
                    BackendError::new(ErrorKind::RateLimited { retry_after: None }, "429"),
                ),
            ],
            ..GroupOutcome::default()
        });
        assert!(
            matches!(&none, TransmitOutcome::Failed(e) if matches!(e.kind, ErrorKind::RateLimited { .. })),
            "the retryable failure wins: {none:?}"
        );
    }

    /// The key setup and the 1:1 sends of a group run several members at once, never more
    /// than the bound: 40 sends of 20 ms each at 16 at once take three rounds, not forty.
    #[tokio::test(flavor = "current_thread")]
    async fn member_steps_run_at_once_up_to_the_bound() {
        use std::cell::Cell;
        let in_flight = Cell::new(0_usize);
        let most = Cell::new(0_usize);
        let started = tokio::time::Instant::now();
        let done = bounded(0..40_u32, MEMBER_SENDS_AT_ONCE, |n| {
            let (in_flight, most) = (&in_flight, &most);
            async move {
                in_flight.set(in_flight.get() + 1);
                most.set(most.get().max(in_flight.get()));
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                in_flight.set(in_flight.get() - 1);
                n
            }
        })
        .await;
        let mut done = done;
        done.sort_unstable();
        assert_eq!(done, (0..40).collect::<Vec<_>>(), "every member, once");
        assert_eq!(
            most.get(),
            MEMBER_SENDS_AT_ONCE,
            "bounded, and actually parallel"
        );
        // 60 ms in three rounds; one at a time would be 800 ms.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(400),
            "{:?}",
            started.elapsed()
        );
        assert!(
            (20..=50).contains(&PREKEY_FETCHES_AT_ONCE),
            "Desktop's range"
        );
        assert!(
            (10..=20).contains(&MEMBER_SENDS_AT_ONCE),
            "the lead's range"
        );
    }

    #[test]
    fn update_keys_name_the_members() {
        let a = update_key("100", &[(aci(1), true), (aci(2), true)]);
        let b = update_key("100", &[(aci(2), false), (aci(1), true)]);
        assert_eq!(a, b, "order and sealing do not matter");
        assert!(a.starts_with("100:update:"));
        assert_ne!(a, update_key("100", &[(aci(1), true)]));
    }
}
