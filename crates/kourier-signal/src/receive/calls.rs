//! Calls: what a device that takes no calls is told about them, as call lines
//! (`IncomingMessage::call`, whose table is the contract; the evidence is in the project
//! notes, "Calls shown, not taken").
//!
//! NOTHING IS EVER SENT FOR A CALL. Nothing here queues a job, a receipt or a sync, and
//! nothing may: a Busy, a Hangup or a ring response from one of the user's devices can end the
//! call or stop the phone ringing (`SD/ts/services/calling.preload.ts` `handleCallingMessage`
//! and `handleGroupCallRingUpdate` are silent whenever Desktop does not take the call).
//!
//! | On the wire | Reported as |
//! |---|---|
//! | `CallMessage.offer` of a contact | incoming 1:1 line, ringing for what is left of [`DIRECT_RING_MS`], voice or video, by the caller |
//! | the same from a caller whose chat is not accepted | the same line with no time left and `silent` (the phone answers such a caller itself; Desktop writes a missed call) |
//! | `CallMessage.hangup` NORMAL | incoming 1:1 line, missed (the caller gave up) |
//! | `CallMessage.hangup` of any other type | nothing: it says what another of our devices did, which only that device may say |
//! | `CallMessage.opaque` with RingRTC's `ring_intention` RING | incoming group line, ringing for what is left of [`GROUP_RING_MS`], by the ringer |
//! | the same, CANCELLED | incoming group line, missed |
//! | `CallMessage` answer, ICE update, busy, an opaque payload without a ring | nothing |
//! | `DataMessage.groupCallUpdate` of a member | incoming group line, started, by its sender |
//! | the same in our own sync transcript | incoming group line, answered, naming nobody |
//! | `SyncMessage.callEvent` | [`synced_call`] |
//!
//! The opaque payload is read first: a message with an Offer AND a ring is a group ring
//! (Desktop's own test fixture sends both, `SD/ts/test-mock/calling/`), never a 1:1 call of the
//! ringer beside it.
//!
//! Ids: a 1:1 call is the Offer's id, which is the `callId` of the phone's sync. A group call is
//! its ring id as an unsigned decimal ([`call_id_text`]: the phone syncs `CallId(ringId)`), and
//! an era id maps to the same number ([`ring_id_of_era`]), so a ring, a group-call update and
//! the phone's sync of one call meet in one line.
//!
//! The age of a ring ([`age_ms`]) is measured on the server's clock at both ends, as Desktop
//! measures it (`SD/ts/textsecure/MessageReceiver.preload.ts` `#calculateMessageAge`): the
//! time the server delivered the envelope minus the time it took it. This computer's clock is
//! not in it, so a wrong local clock neither shortens nor lengthens a ring.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::rusqlite::Connection;
use kourier_model::{CallDirection, CallLine, CallMedia, CallState, ConvKey, Origin, UserId};
use libsignal_core::{Aci, ServiceId};
use prost::Message as _;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::dispatch::{Ctx, Outcome, Who};
use super::group_dispatch;
use super::stage2::Row;
use crate::error::Result;
use crate::proto::{self, ringrtc};
use crate::service_id::aci_user_id;
use crate::{features, groups};

/// How long a 1:1 call rings (ms): RingRTC's `TIME_OUT_PERIOD`, which is also its
/// `MAX_MESSAGE_AGE` (an Offer older than this is expired). Read in signalapp/ringrtc
/// `src/rust/src/core/call_manager.rs` on 2026-10-09; RingRTC is on no disk here.
pub(crate) const DIRECT_RING_MS: i64 = 60_000;

/// How long the ring of a group call lasts (ms): the one constant both reference apps hold
/// (`SD/ts/components/CallManager.dom.tsx` `GROUP_CALL_RING_DURATION`; Android
/// `CallParticipantsState.kt` `MAX_OUTGOING_GROUP_RING_DURATION`).
pub(crate) const GROUP_RING_MS: i64 = 60_000;

/// The ring id an era id stands for, exactly as RingRTC's `RingId::from_era_id`
/// (signalapp/ringrtc `src/rust/src/core/group_call.rs`, read on 2026-10-09): an era id of 16
/// bytes that `u64::from_str_radix(_, 16)` reads is that number as an i64, except that 0
/// becomes -1; every other era id is the first 8 bytes of its SHA-256, little-endian.
pub(crate) fn ring_id_of_era(era_id: &str) -> i64 {
    if era_id.len() == 16
        && let Ok(number) = u64::from_str_radix(era_id, 16)
    {
        return match number.cast_signed() {
            0 => -1,
            id => id,
        };
    }
    let digest = Sha256::digest(era_id.as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    i64::from_le_bytes(first)
}

/// The call id of a group line: the ring id read as an unsigned number, in decimal (what the
/// phone's `CallEvent.callId`, a uint64, prints as).
pub(crate) fn call_id_text(ring_id: i64) -> String {
    ring_id.cast_unsigned().to_string()
}

/// The ring of a group call in a `CallMessage.opaque.data`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GroupRing {
    /// The group that is rung.
    pub(crate) group_id: Vec<u8>,
    /// The ring's id (the call's id once read as unsigned).
    pub(crate) ring_id: i64,
    /// The ringer took the ring back (`CANCELLED`); otherwise it rings (`RING`).
    pub(crate) cancelled: bool,
}

/// A ring intention this version cannot read: a type it does not know, or one without its
/// group or its id. It is still a group ring, so whatever stands beside it (an Offer) is not a
/// 1:1 call of the ringer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnreadableRing;

/// Reads RingRTC's `signaling.CallMessage { ring_intention }` from an opaque payload.
/// `Ok(None)` when there is no ring intention at all: bytes that are no such message, another
/// kind of RingRTC signalling, an empty payload. `Err` when there IS a ring intention that
/// this version cannot read.
pub(crate) fn decode_ring(opaque: &[u8]) -> Result<Option<GroupRing>, UnreadableRing> {
    use ringrtc::call_message::ring_intention::Type;
    let Some(ring) = ringrtc::CallMessage::decode(opaque)
        .ok()
        .and_then(|m| m.ring_intention)
    else {
        return Ok(None);
    };
    let cancelled = match ring.r#type {
        // proto2: an absent type is the default, RING.
        None => false,
        Some(kind) => match Type::try_from(kind).map_err(|_| UnreadableRing)? {
            Type::Ring => false,
            Type::Cancelled => true,
        },
    };
    Ok(Some(GroupRing {
        group_id: ring
            .group_id
            .filter(|id| !id.is_empty())
            .ok_or(UnreadableRing)?,
        ring_id: ring.ring_id.ok_or(UnreadableRing)?,
        cancelled,
    }))
}

/// How long an envelope waited on the server (ms): the server's clock when it delivered the
/// envelope (`delivered_at`, `x-signal-timestamp` of the delivery) minus the server's clock
/// when it took it (`server_timestamp`). Zero when either is missing or the delivery is not
/// later, as Desktop's `#calculateMessageAge` ("default to 0 in case of unreliable
/// parameters"): the ring then lasts its full time from the receipt, never longer.
pub(crate) fn age_ms(server_timestamp: Option<i64>, delivered_at: Option<i64>) -> i64 {
    match (server_timestamp, delivered_at) {
        (Some(taken), Some(delivered)) if taken > 0 && delivered > taken => delivered - taken,
        _ => 0,
    }
}

/// The time a ring of `ring_ms` has left when its signal is `age_ms` old; zero or less: over.
pub(crate) fn time_left(ring_ms: i64, age_ms: i64) -> i64 {
    ring_ms.saturating_sub(age_ms)
}

/// What a `SyncMessage.callEvent` of our own devices says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Synced {
    /// A signal of the call's line.
    Line {
        /// A group call.
        group: bool,
        /// Who called whom.
        direction: CallDirection,
        /// What the call became.
        state: CallState,
        /// Voice or video (1:1 only).
        media: Option<CallMedia>,
        /// The line names the peer as the caller (an incoming 1:1 call).
        by_peer: bool,
    },
    /// The user deleted the call's entry.
    Delete {
        /// A group call.
        group: bool,
    },
    /// Nothing to show (log-safe reason).
    Ignored(&'static str),
}

/// The mapping of `CallEvent { type, direction, event }` (the wire values), per the contract:
///
/// | type | direction | event | |
/// |---|---|---|---|
/// | AUDIO, VIDEO | INCOMING | ACCEPTED | incoming, answered, by the peer |
/// | AUDIO, VIDEO | INCOMING | NOT_ACCEPTED | incoming, declined, by the peer (Android sends it only when the user pressed decline) |
/// | AUDIO, VIDEO | OUTGOING | ACCEPTED | outgoing, answered |
/// | AUDIO, VIDEO | OUTGOING | NOT_ACCEPTED | outgoing, unanswered |
/// | GROUP | either | ACCEPTED | incoming group call, answered, naming nobody (the phone's "joined" goes out as ACCEPTED: the wire has no other value for it) |
/// | GROUP | INCOMING | NOT_ACCEPTED | incoming group call, declined, naming nobody |
/// | GROUP | OUTGOING | NOT_ACCEPTED | ignored (Android: "invalid direction") |
/// | AUDIO, VIDEO, GROUP | any | DELETE | the line is deleted |
/// | any | any | OBSERVED, unknown | ignored |
/// | AD_HOC (a call link), unknown | | | ignored |
pub(crate) fn synced_call(kind: Option<i32>, direction: Option<i32>, event: Option<i32>) -> Synced {
    use proto::sync_message::call_event::{Direction, Event, Type};
    let (group, media) = match kind.and_then(|k| Type::try_from(k).ok()) {
        Some(Type::AudioCall) => (false, Some(CallMedia::Voice)),
        Some(Type::VideoCall) => (false, Some(CallMedia::Video)),
        Some(Type::GroupCall) => (true, None),
        Some(Type::AdHocCall) => return Synced::Ignored("call event of a call link"),
        Some(Type::UnknownType) | None => return Synced::Ignored("call event of an unknown type"),
    };
    let accepted = match event.and_then(|e| Event::try_from(e).ok()) {
        Some(Event::Accepted) => true,
        Some(Event::NotAccepted) => false,
        Some(Event::Delete) => return Synced::Delete { group },
        Some(Event::Observed) => return Synced::Ignored("call event OBSERVED"),
        Some(Event::UnknownEvent) | None => {
            return Synced::Ignored("call event of an unknown kind");
        }
    };
    let incoming = match direction.and_then(|d| Direction::try_from(d).ok()) {
        Some(Direction::Incoming) => true,
        Some(Direction::Outgoing) => false,
        Some(Direction::UnknownDirection) | None => {
            return Synced::Ignored("call event without a direction");
        }
    };
    let (direction, state) = match (group, incoming, accepted) {
        (true, _, true) | (false, true, true) => (CallDirection::Incoming, CallState::Answered),
        (_, true, false) => (CallDirection::Incoming, CallState::Declined),
        (true, false, false) => {
            return Synced::Ignored("call event: an outgoing group call not accepted");
        }
        (false, false, true) => (CallDirection::Outgoing, CallState::Answered),
        (false, false, false) => (CallDirection::Outgoing, CallState::Unanswered),
    };
    Synced::Line {
        group,
        direction,
        state,
        media,
        by_peer: !group && direction == CallDirection::Incoming,
    }
}

/// When a signal was made, for what the OTHER side says and for a sync that states no time of
/// its own: the server's clock when it took the envelope, which the sender cannot choose (a
/// line shows who its EARLIEST signal names, so with the sender's own time stamp a group
/// member could put his name on another member's call and move it into the past). The
/// envelope's own time stamp only when the server stated none.
fn sent_at(row: &Row) -> i64 {
    row.server_timestamp
        .filter(|at| *at > 0)
        .unwrap_or_else(|| i64::try_from(row.timestamp).unwrap_or(i64::MAX))
}

/// The time a ring of `ring_ms` has left. An envelope from before the catch-up boundary whose
/// delivery the server did not time waited for an unknown while: it never rings.
fn ring_left(ctx: &Ctx<'_>, row: &Row, ring_ms: i64) -> i64 {
    if row.delivered_at.is_none() && ctx.catch_up {
        return 0;
    }
    time_left(ring_ms, age_ms(row.server_timestamp, row.delivered_at))
}

/// The call signal as a durable event, with what stage 1 stored of its envelope.
/// `received_at` is the row's (the same on every replay), and `catch_up` says whether the
/// signal is from before the catch-up boundary, exactly as for a message.
fn event(ctx: &Ctx<'_>, row: &Row, mut message: IncomingMessage) -> BackendEvent {
    message.server_at = row.server_timestamp;
    message.arrival = Some(row.arrival);
    message.catch_up = ctx.catch_up;
    BackendEvent::Message(message)
}

/// A `CallMessage` (`SD/ts/textsecure/MessageReceiver.preload.ts` `#handleCallingMessage`,
/// `SD/ts/services/calling.preload.ts` `handleCallingMessage`). Reads only; sends and queues
/// nothing.
pub(super) fn call_message(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    sender: Option<ServiceId>,
    message: &proto::CallMessage,
) -> Result<Outcome> {
    let Some(ServiceId::Aci(aci)) = sender else {
        return Ok(Outcome::dropped("call message from a non-ACI sender"));
    };
    if aci == ctx.account.aci {
        return Ok(Outcome::dropped("call message from our own account"));
    }
    if message
        .destination_device_id
        .is_some_and(|device| device != u32::from(ctx.account.device_id))
    {
        return Ok(Outcome::dropped("call message for another of our devices"));
    }
    let Ok(caller) = aci_user_id(aci) else {
        return Ok(Outcome::dropped("sender id"));
    };
    // The opaque payload first: a ring that comes with an Offer is a group ring, also one
    // this version cannot read (its Offer must not become a 1:1 call of the ringer).
    let opaque = message.opaque.as_ref().and_then(|o| o.data.as_deref());
    match opaque.map(decode_ring) {
        Some(Ok(Some(ring))) => return group_ring(conn, ctx, row, aci, caller, &ring),
        Some(Err(UnreadableRing)) => {
            return Ok(Outcome::dropped("ring intention this version cannot read"));
        }
        Some(Ok(None)) | None => {}
    }
    if let Some(offer) = &message.offer {
        return offer_line(conn, ctx, row, aci, caller, offer);
    }
    if let Some(hangup) = &message.hangup {
        return hangup_line(conn, ctx, row, aci, caller, hangup);
    }
    Ok(Outcome::dropped(if opaque.is_some() {
        "opaque call message without a ring"
    } else {
        "call signalling that is not shown (answer, ICE update, busy)"
    }))
}

/// A 1:1 Offer: the call rings for what is left of [`DIRECT_RING_MS`]. A caller whose chat is
/// not accepted gets no ringing phase and no notification: the phone answers such a caller
/// itself (need-permission) and Desktop writes a missed call
/// (`calling.preload.ts:3188-3252`); what this device knows of the chat is `peers`
/// (`features::is_accepted`).
fn offer_line(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    caller: UserId,
    offer: &proto::call_message::Offer,
) -> Result<Outcome> {
    use proto::call_message::offer::Type;
    let Some(id) = offer.id else {
        return Ok(Outcome::dropped("offer without a call id"));
    };
    let media = match offer.r#type() {
        Type::OfferAudioCall => CallMedia::Voice,
        Type::OfferVideoCall => CallMedia::Video,
    };
    let accepted = features::is_accepted(conn, ctx.account, ServiceId::from(aci))?;
    let left = if accepted {
        ring_left(ctx, row, DIRECT_RING_MS)
    } else {
        0
    };
    let at = sent_at(row);
    let line = CallLine::signal(
        CallDirection::Incoming,
        false,
        CallState::Ringing {
            deadline: at.saturating_add(left),
        },
        at,
    )
    .with_media(media, at)
    .with_by(caller.clone(), at);
    let mut message = IncomingMessage::call(
        ConvKey::Direct(caller.clone()),
        &id.to_string(),
        Origin::Remote {
            author: caller,
            device: Some(row.sender_device),
        },
        line,
        at,
        row.received_at,
    );
    message.silent = !accepted;
    Ok(Outcome::events(vec![event(ctx, row, message)]))
}

/// The caller's Hangup. Only NORMAL (the caller gave up) is reported, as a missed call. Every
/// other type says what another of OUR devices did (accepted, declined, busy) or that the
/// phone asked for permission: the peer cannot say what the user did, and reporting it as
/// missed would announce every call answered on the phone as missed until the phone's sync
/// arrives. The phone's sync, or the ring's deadline, settles those lines.
fn hangup_line(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    caller: UserId,
    hangup: &proto::call_message::Hangup,
) -> Result<Outcome> {
    use proto::call_message::hangup::Type;
    let normal = match hangup.r#type {
        // proto2: an absent type is the default, HANGUP_NORMAL.
        None => true,
        Some(kind) => Type::try_from(kind) == Ok(Type::HangupNormal),
    };
    if !normal {
        return Ok(Outcome::dropped(
            "hangup that is not the caller giving up (not reported)",
        ));
    }
    let Some(id) = hangup.id else {
        return Ok(Outcome::dropped("hangup without a call id"));
    };
    let at = sent_at(row);
    let line = CallLine::signal(CallDirection::Incoming, false, CallState::Missed, at)
        .with_by(caller.clone(), at);
    let mut message = IncomingMessage::call(
        ConvKey::Direct(caller.clone()),
        &id.to_string(),
        Origin::Remote {
            author: caller,
            device: Some(row.sender_device),
        },
        line,
        at,
        row.received_at,
    );
    // As for the Offer of such a caller: a line, never an announcement.
    message.silent = !features::is_accepted(conn, ctx.account, ServiceId::from(aci))?;
    Ok(Outcome::events(vec![event(ctx, row, message)]))
}

/// The ring of a group call, or its cancellation. No line when the group is unknown, blocked
/// or ended, when we are not a member, when the ringer is not, and for a non-admin in an
/// announcement-only group (`calling.preload.ts` `handleGroupCallRingUpdate` writes nothing
/// for these either).
fn group_ring(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    ringer: UserId,
    ring: &GroupRing,
) -> Result<Outcome> {
    let group = groups::load_by_id(conn, &ring.group_id)?;
    let keys = match group_dispatch::admit_call(group.as_ref(), aci) {
        Ok(keys) => keys,
        Err(note) => return Ok(Outcome::dropped(note)),
    };
    let at = sent_at(row);
    let state = if ring.cancelled {
        CallState::Missed
    } else {
        let left = ring_left(ctx, row, GROUP_RING_MS);
        CallState::Ringing {
            deadline: at.saturating_add(left),
        }
    };
    let line =
        CallLine::signal(CallDirection::Incoming, true, state, at).with_by(ringer.clone(), at);
    let message = IncomingMessage::call(
        keys.conv_key(),
        &call_id_text(ring.ring_id),
        Origin::Remote {
            author: ringer,
            device: Some(row.sender_device),
        },
        line,
        at,
        row.received_at,
    );
    Ok(Outcome::events(vec![event(ctx, row, message)]))
}

/// `DataMessage.groupCallUpdate` in the group `conv`, sent at `timestamp` (the caller checked
/// the group and its sender as for any group message). From a member: somebody started or
/// joined a call nobody rang us for. In our own sync transcript: the user is in the call on
/// another device, which is "answered" and names nobody. Without an era id (Android sends
/// that when a ring is declined) there is no call to name: ignored. One line per call,
/// however many updates it has: they all map to the same id.
pub(super) fn group_call_update(
    ctx: &Ctx<'_>,
    row: &Row,
    who: &Who,
    conv: ConvKey,
    update: &proto::data_message::GroupCallUpdate,
    timestamp: u64,
) -> Outcome {
    let Some(era) = update.era_id.as_deref().filter(|era| !era.is_empty()) else {
        return Outcome::dropped("group-call update without an era id");
    };
    let at = i64::try_from(timestamp).unwrap_or(i64::MAX);
    let line = match who {
        Who::Remote { user, .. } => {
            CallLine::signal(CallDirection::Incoming, true, CallState::Started, at)
                .with_by(user.clone(), at)
        }
        Who::Own { .. } => CallLine::signal(CallDirection::Incoming, true, CallState::Answered, at),
    };
    let message = IncomingMessage::call(
        conv,
        &call_id_text(ring_id_of_era(era)),
        who.origin(),
        line,
        at,
        row.received_at,
    );
    Outcome::events(vec![event(ctx, row, message)])
}

/// `SyncMessage.callEvent` of one of our own devices ([`synced_call`]; the caller checked that
/// it is one). A 16-byte `conversationId` is the peer's ACI, anything else a group id; an
/// event whose type does not fit that (a group call with a contact, a 1:1 call in a group) is
/// ignored, as both reference apps drop a mismatch. A group the core does not have gets no
/// line (a line must not open a chat). The line's time is the event's own `timestamp` (the
/// phone's clock when the call connected or was declined).
pub(super) fn call_event(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    event_message: &proto::sync_message::CallEvent,
) -> Result<Outcome> {
    let synced = synced_call(
        event_message.r#type,
        event_message.direction,
        event_message.event,
    );
    let group = match &synced {
        Synced::Ignored(note) => return Ok(Outcome::dropped(note)),
        Synced::Line { group, .. } | Synced::Delete { group } => *group,
    };
    let (Some(conversation), Some(call_id)) = (
        event_message.conversation_id.as_deref(),
        event_message.call_id,
    ) else {
        return Ok(Outcome::dropped(
            "call event without a conversation or a call id",
        ));
    };
    let (conv, peer) = match Uuid::from_slice(conversation) {
        Ok(_) if group => {
            return Ok(Outcome::dropped("group call event that names a contact"));
        }
        Ok(uuid) => {
            let aci = Aci::from(uuid);
            if aci == ctx.account.aci {
                return Ok(Outcome::dropped("call event that names our own account"));
            }
            let Ok(peer) = aci_user_id(aci) else {
                return Ok(Outcome::dropped("call event peer id"));
            };
            // As for a group below, and as Desktop does: a line never opens a chat.
            if crate::peers::load(conn, &ServiceId::from(aci).service_id_string())?
                .features_hash
                .is_none()
            {
                return Ok(Outcome::dropped(
                    "call event for a contact the core does not have",
                ));
            }
            (ConvKey::Direct(peer.clone()), Some(peer))
        }
        Err(_) if !group => {
            return Ok(Outcome::dropped("1:1 call event that names a group"));
        }
        Err(_) => match groups::load_by_id(conn, conversation)? {
            Some(known) if known.features_hash.is_some() => (known.keys.conv_key(), None),
            _ => {
                return Ok(Outcome::dropped(
                    "call event for a group the core does not have",
                ));
            }
        },
    };
    let at = event_message
        .timestamp
        .filter(|t| *t > 0)
        .map_or_else(|| sent_at(row), |t| i64::try_from(t).unwrap_or(i64::MAX));
    let id = call_id.to_string();
    Ok(match synced {
        Synced::Ignored(note) => Outcome::dropped(note),
        Synced::Delete { group } => Outcome::events(vec![BackendEvent::Deleted {
            target: IncomingMessage::call_target(conv, group, &id),
            by: None,
            for_everyone: false,
            at,
        }]),
        Synced::Line {
            group,
            direction,
            state,
            media,
            by_peer,
        } => {
            let mut line = CallLine::signal(direction, group, state, at);
            if let Some(media) = media {
                line = line.with_media(media, at);
            }
            if let (true, Some(peer)) = (by_peer, peer) {
                line = line.with_by(peer, at);
            }
            let message = IncomingMessage::call(
                conv,
                &id,
                Origin::OwnOtherDevice {
                    device: Some(row.sender_device),
                },
                line,
                at,
                row.received_at,
            );
            Outcome::events(vec![event(ctx, row, message)])
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first eight bytes of a SHA-256 as RingRTC reads them: little-endian, signed.
    fn le_i64(first: [u8; 8]) -> i64 {
        let mut value: u64 = 0;
        for (i, byte) in first.iter().enumerate() {
            value |= u64::from(*byte) << (8 * i);
        }
        i64::from_ne_bytes(value.to_ne_bytes())
    }

    #[test]
    fn an_era_id_maps_to_its_ring_id_as_ringrtc_does_it() {
        // Sixteen hex digits: that number.
        assert_eq!(ring_id_of_era("0000000000000001"), 1);
        assert_eq!(ring_id_of_era("00000000075bcd15"), 123_456_789);
        assert_eq!(ring_id_of_era("7fffffffffffffff"), i64::MAX);
        // Above i64::MAX the number wraps into the negative half, and prints unsigned again.
        assert_eq!(ring_id_of_era("8000000000000000"), i64::MIN);
        assert_eq!(ring_id_of_era("ffffffffffffffff"), -1);
        assert_eq!(
            ring_id_of_era("FFFFFFFFFFFFFFFE"),
            -2,
            "upper case is hex too"
        );
        assert_eq!(
            call_id_text(ring_id_of_era("ffffffffffffffff")),
            "18446744073709551615"
        );
        assert_eq!(
            call_id_text(ring_id_of_era("8000000000000000")),
            "9223372036854775808"
        );
        // Zero is no ring id: -1.
        assert_eq!(ring_id_of_era("0000000000000000"), -1);
        assert_eq!(
            call_id_text(ring_id_of_era("0000000000000000")),
            "18446744073709551615"
        );

        // Everything else: the first 8 bytes of its SHA-256, little-endian. The digests are
        // the published test vectors of SHA-256 (FIPS 180-4 "abc" and the empty message), not
        // computed by the code under test.
        // SHA-256("abc") = ba7816bf 8f01cfea 414140de …
        let abc = le_i64([0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea]);
        assert_eq!(ring_id_of_era("abc"), abc);
        assert_eq!(abc.cast_unsigned(), 0xeacf_018f_bf16_78ba);
        assert_eq!(call_id_text(ring_id_of_era("abc")), "16919744041952114874");
        // SHA-256("") = e3b0c442 98fc1c14 9afbf4c8 …
        let empty = le_i64([0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14]);
        assert_eq!(ring_id_of_era(""), empty);
        assert_eq!(empty, 0x141c_fc98_42c4_b0e3);

        // Sixteen characters that are no hex number are hashed like any other era id, and
        // so are hex digits of another length.
        let not_hex = "ghijklmnopqrstuv";
        assert_eq!(not_hex.len(), 16);
        let digest = Sha256::digest(not_hex.as_bytes());
        let mut first = [0u8; 8];
        first.copy_from_slice(&digest[..8]);
        assert_eq!(ring_id_of_era(not_hex), le_i64(first));
        assert_ne!(ring_id_of_era("00000000000000001"), 1, "17 digits");
        assert_ne!(ring_id_of_era("000000000000001"), 1, "15 digits");
        assert_ne!(ring_id_of_era("000000000000000g"), 16);
    }

    fn ring_bytes(group_id: Option<&[u8]>, kind: Option<i32>, ring_id: Option<i64>) -> Vec<u8> {
        ringrtc::CallMessage {
            group_call_message: None,
            ring_intention: Some(ringrtc::call_message::RingIntention {
                group_id: group_id.map(<[u8]>::to_vec),
                r#type: kind,
                ring_id,
            }),
        }
        .encode_to_vec()
    }

    #[test]
    fn a_ring_is_read_from_the_opaque_payload_and_nothing_else_is() {
        let group = [7u8; 32];
        assert_eq!(
            decode_ring(&ring_bytes(Some(&group), Some(0), Some(42))),
            Ok(Some(GroupRing {
                group_id: group.to_vec(),
                ring_id: 42,
                cancelled: false
            }))
        );
        assert_eq!(
            decode_ring(&ring_bytes(Some(&group), Some(1), Some(-5))),
            Ok(Some(GroupRing {
                group_id: group.to_vec(),
                ring_id: -5,
                cancelled: true
            }))
        );
        // The wire bytes, written by hand: field 2 (ring_intention) of 45 bytes holding
        // field 1 (group_id, 32 bytes), field 2 (type = 1) and field 3 (sfixed64, the id
        // little-endian).
        let mut by_hand = vec![0x12, 45, 0x0a, 32];
        by_hand.extend_from_slice(&group);
        by_hand.extend_from_slice(&[0x10, 1, 0x19]);
        by_hand.extend_from_slice(&(-2i64).to_le_bytes());
        assert_eq!(
            decode_ring(&by_hand),
            Ok(Some(GroupRing {
                group_id: group.to_vec(),
                ring_id: -2,
                cancelled: true
            }))
        );
        assert_eq!(call_id_text(-2), "18446744073709551614");
        // An absent type is RING (proto2's default).
        assert_eq!(
            decode_ring(&ring_bytes(Some(&group), None, Some(9))).map(|r| r.map(|r| r.cancelled)),
            Ok(Some(false))
        );

        // No ring intention at all: an empty payload, another RingRTC message, garbage.
        assert_eq!(decode_ring(&[]), Ok(None));
        let other = ringrtc::CallMessage {
            group_call_message: Some(ringrtc::DeviceToDevice {
                group_id: Some(group.to_vec()),
            }),
            ring_intention: None,
        }
        .encode_to_vec();
        assert_eq!(decode_ring(&other), Ok(None));
        assert_eq!(decode_ring(&[0xff, 0xff, 0xff, 0xff, 0x07]), Ok(None));
        assert_eq!(decode_ring(b"not a protobuf at all"), Ok(None));
        // A ring intention this version cannot read (a type from the future, no group, no
        // id) is still a ring: it is refused as one, not passed over.
        for unreadable in [
            ring_bytes(Some(&group), Some(2), Some(1)),
            ring_bytes(None, Some(0), Some(1)),
            ring_bytes(Some(&[]), Some(0), Some(1)),
            ring_bytes(Some(&group), Some(0), None),
        ] {
            assert_eq!(decode_ring(&unreadable), Err(UnreadableRing));
        }
    }

    #[test]
    fn the_age_of_a_ring_is_measured_on_the_servers_clock() {
        const TAKEN: i64 = 1_700_000_000_000;
        let left = |taken: i64, delivered: i64| {
            time_left(DIRECT_RING_MS, age_ms(Some(taken), Some(delivered)))
        };
        // Fresh: delivered the moment it was taken, or 120 ms later.
        assert_eq!(left(TAKEN, TAKEN), 60_000);
        assert_eq!(left(TAKEN, TAKEN + 120), 59_880);
        // 59 s on the server: one second left. 60 s and 61 s: nothing (zero or less).
        assert_eq!(left(TAKEN, TAKEN + 59_000), 1_000);
        assert_eq!(left(TAKEN, TAKEN + 60_000), 0);
        assert_eq!(left(TAKEN, TAKEN + 61_000), -1_000);
        // Hours in the queue while Kourier was closed.
        assert!(left(TAKEN, TAKEN + 3 * 3_600_000) < 0);

        // The clock of this computer is in neither number: with the server an hour ahead of
        // us, or a day behind, the age is the same.
        for skew in [3_600_000, -86_400_000] {
            assert_eq!(
                age_ms(Some(TAKEN + skew), Some(TAKEN + skew + 59_000)),
                59_000
            );
        }

        // Unreliable parameters count as no age (Desktop's rule): no delivery time, no
        // server time, a delivery before the envelope was taken.
        assert_eq!(age_ms(Some(TAKEN), None), 0);
        assert_eq!(age_ms(None, Some(TAKEN)), 0);
        assert_eq!(age_ms(None, None), 0);
        assert_eq!(age_ms(Some(0), Some(TAKEN)), 0);
        assert_eq!(age_ms(Some(TAKEN), Some(TAKEN - 5_000)), 0);
        // And nothing overflows.
        assert_eq!(time_left(GROUP_RING_MS, i64::MAX), 60_000 - i64::MAX);
        assert_eq!(time_left(i64::MIN, i64::MAX), i64::MIN);
    }

    #[test]
    fn every_call_event_maps_as_the_contract_says() {
        use proto::sync_message::call_event::{Direction, Event, Type};
        let line = |group, direction, state, media, by_peer| Synced::Line {
            group,
            direction,
            state,
            media,
            by_peer,
        };
        let (voice, video) = (Some(CallMedia::Voice), Some(CallMedia::Video));
        let (inc, out) = (CallDirection::Incoming, CallDirection::Outgoing);
        let types = [
            Type::UnknownType,
            Type::AudioCall,
            Type::VideoCall,
            Type::GroupCall,
            Type::AdHocCall,
        ];
        let directions = [
            Direction::UnknownDirection,
            Direction::Incoming,
            Direction::Outgoing,
        ];
        let events = [
            Event::UnknownEvent,
            Event::Accepted,
            Event::NotAccepted,
            Event::Delete,
            Event::Observed,
        ];
        let mut lines = 0;
        let mut deletes = 0;
        for kind in types {
            for direction in directions {
                for event in events {
                    let got = synced_call(
                        Some(kind.into()),
                        Some(direction.into()),
                        Some(event.into()),
                    );
                    let media = if kind == Type::VideoCall {
                        video
                    } else {
                        voice
                    };
                    let expected = match (kind, direction, event) {
                        (Type::UnknownType | Type::AdHocCall, _, _)
                        | (_, _, Event::UnknownEvent | Event::Observed) => None,
                        (Type::GroupCall, _, Event::Delete) => Some(Synced::Delete { group: true }),
                        (_, _, Event::Delete) => Some(Synced::Delete { group: false }),
                        (_, Direction::UnknownDirection, _) => None,
                        (Type::GroupCall, _, Event::Accepted) => {
                            Some(line(true, inc, CallState::Answered, None, false))
                        }
                        (Type::GroupCall, Direction::Incoming, Event::NotAccepted) => {
                            Some(line(true, inc, CallState::Declined, None, false))
                        }
                        (Type::GroupCall, Direction::Outgoing, Event::NotAccepted) => None,
                        (_, Direction::Incoming, Event::Accepted) => {
                            Some(line(false, inc, CallState::Answered, media, true))
                        }
                        (_, Direction::Incoming, Event::NotAccepted) => {
                            Some(line(false, inc, CallState::Declined, media, true))
                        }
                        (_, Direction::Outgoing, Event::Accepted) => {
                            Some(line(false, out, CallState::Answered, media, false))
                        }
                        (_, Direction::Outgoing, Event::NotAccepted) => {
                            Some(line(false, out, CallState::Unanswered, media, false))
                        }
                    };
                    match (&got, &expected) {
                        (Synced::Ignored(_), None) => {}
                        (got, Some(expected)) => {
                            assert_eq!(got, expected, "{kind:?} {direction:?} {event:?}");
                        }
                        (got, None) => panic!("{kind:?} {direction:?} {event:?}: {got:?}"),
                    }
                    match got {
                        Synced::Line { .. } => lines += 1,
                        Synced::Delete { .. } => deletes += 1,
                        Synced::Ignored(_) => {}
                    }
                }
            }
        }
        // 2 kinds x 2 directions x 2 outcomes for 1:1, three for a group; a delete in every
        // direction of the three kinds of call.
        assert_eq!((lines, deletes), (8 + 3, 9));

        // Absent and unknown wire values are ignored, never read as a default.
        let accepted = Some(Event::Accepted.into());
        let incoming = Some(Direction::Incoming.into());
        let audio = Some(Type::AudioCall.into());
        for got in [
            synced_call(None, incoming, accepted),
            synced_call(audio, None, accepted),
            synced_call(audio, incoming, None),
            synced_call(Some(99), incoming, accepted),
            synced_call(audio, Some(99), accepted),
            synced_call(audio, incoming, Some(99)),
        ] {
            assert!(matches!(got, Synced::Ignored(_)), "{got:?}");
        }
    }
}
