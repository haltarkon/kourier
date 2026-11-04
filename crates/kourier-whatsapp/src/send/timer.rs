//! The 1:1 disappearing timer set from Kourier (W6a2, B7b, decision D22; his M1 parity: Signal sets it too).
//!
//! `Command::ChangeConversation { SetTimer }` for a DIRECT chat is the one conversation change W6a2 writes to WhatsApp, and
//! its frame is the second of the only two W6a2 sends (the other is the admin revoke): ONE `protocolMessage
//! {EPHEMERAL_SETTING}` (`content::timer_setting`, whatsmeow's `SetDisappearingTimer`) through the ONE `Client::send`
//! (`RealSession::send`), only on the user's command, never in reaction to anything the network said. A group's timer is
//! an admin's group change and stays K8c; every other conversation change is still refused (W6b).
//!
//! WHICH VALUES: the three the apps offer (`features::TIMER_CHOICES`: 24 hours, 7 days, 90 days) and off. WhatsApp's apps
//! ignore any other value (whatsmeow `send.go:650-654`), so one is refused here with no frame ([`request`]). The core only
//! asks for what the chat's features take (`takes_timer`: a direct chat states the three), so this is the second line.
//!
//! THE ORDER, every step before the next (the same gates as a message, `send::attempt`):
//! 1. one store read: the started fact and the token facts.
//! 2. D6 in the backend: a chat the phone never started is refused (`START_ON_PHONE`), no frame.
//! 3. the hold (D8): while the reach-out lock is unknown or held, nothing is written (`Transient{HOLD_RETRY}`).
//! 4. the token gate (D12): a chat with no stored token waits while the link's history may still bring it.
//! 5. our own ids are known and a session is there.
//! 6. the message is built (pure) and the hold is asked AGAIN right before the frame (fix F6).
//! 7. THE POINT OF NO RETURN (`send::stop`): the app's quit that came first ends the change with no frame; one that comes
//!    later waits for the ack like any frame in the air.
//! 8. THE REGISTRY (D5): the wait is registered BEFORE the frame, one frame of the change's id per connection.
//! 9. THE ANSWER: an ack is confirmed to the core in ONE zone of the event writer (the chat's `timer` and `timer_version`,
//!    and the timeline notice `SystemEvent::TimerChanged{by: None}` under the id the core allocated, `Origin::OwnThisDevice`:
//!    the contract of `ChangeConversation`), and ONLY THEN is the command answered `Ok`. A nack fails the command with the
//!    send path's table (`send::errors`), a 463/475 as `ReachOutRefused`; no ack within `ACK_WAIT` is `Transient` and ends the
//!    connection (step 7 of a send), and the change is NOT confirmed: the core shows the old timer and an error.
//!
//! THE VERSION is the frame's `ephemeralSettingTimestamp`, our clock in seconds, as whatsmeow writes it: the number every
//! receiver orders settings by, and the one Kourier's own receive map (`map::modifier::timer_setting`) states for a setting
//! the phone or the peer made, so the core's "an older one loses" rule judges both on one scale. A clock of ours that runs
//! behind the other side's by more than the time since its last change makes the core drop ours: a known limit.
//!
//! NO LEDGER: the core never sends a conversation change again (a `Transmit` is retried, a `ChangeConversation` is not),
//! so there is no resend to answer from `send_outcomes`, and its kinds are those of messages. A frame that was acked while
//! the confirmation could not be written leaves WhatsApp with the new timer and Kourier with the old one until the chat
//! says its timer again (the next message of the peer carries it, the history's chat record): the error says so.
//!
//! Logs: a fixed word and a kind, never an id, a chat or a timer.

use std::time::Duration;

use kourier_backend_api::{AllocatedId, BackendEvent, IncomingMessage};
use kourier_model::{
    BackendError, Body, ConvKey, ConversationPatch, ErrorKind, Origin, Patch, SystemEvent,
};

use super::acks::{Answer, Waited};
use super::address::jid_of;
use super::attempt::{
    Parts, START_ON_PHONE, Written, connection_ended, end_unanswered, held_back, no_ack, refused,
    write_once,
};
use super::content;
use super::errors::{Cause, error_of, nack_error};
use super::ledger::Outcome;
use super::tokens::{self, TOKEN_RETRY};
use super::{started, stop};
use crate::events::EventWriter;
use crate::features::TIMER_CHOICES;
use crate::session::{Devices, Groups, Outgoing, SendFailure};

/// The log word of this kind of frame (`OutgoingKind::name`'s counterpart).
const WHAT: &str = "TimerSet";

/// What the actor accepted of a `SetTimer`: the chat, the id the core allocated for the change and the timer in seconds
/// (0 = off).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Set {
    pub(crate) conv: ConvKey,
    pub(crate) id: AllocatedId,
    pub(crate) secs: u32,
}

/// Judges a `SetTimer` before anything else is done: a direct chat only, a message id from the core, one of the offered
/// timers or off. The refusals are the contract's (`Unsupported` for what Kourier does not do, `InvalidInput` for a
/// value it will not write).
pub(crate) fn request(
    conv: ConvKey,
    timer: Option<Duration>,
    id: Option<AllocatedId>,
) -> Result<Set, BackendError> {
    if !matches!(conv, ConvKey::Direct(_)) {
        // A group's timer is an admin's group change (K8c); the core does not ask for it (no timer is stated for a group).
        return Err(BackendError::unsupported(
            "a group's timer is not set from Kourier yet",
        ));
    }
    let id = id.ok_or_else(|| {
        BackendError::new(ErrorKind::InvalidInput, "a timer change needs a message id")
    })?;
    let secs = match timer {
        None => 0,
        Some(timer) => u32::try_from(timer.as_secs())
            .ok()
            .filter(|secs| timer.subsec_nanos() == 0 && TIMER_CHOICES.contains(secs))
            .ok_or_else(|| {
                BackendError::new(
                    ErrorKind::InvalidInput,
                    "WhatsApp offers a timer of 24 hours, 7 days or 90 days",
                )
            })?,
    };
    Ok(Set { conv, id, secs })
}

/// The two events that confirm a change to the core, in the order the receive map states a setting of the phone's: the
/// chat's timer with its version, then the notice (`by: None` = our own account) under the allocated id. `at_s` is the
/// frame's `ephemeralSettingTimestamp`.
fn confirmation(set: &Set, at_s: i64, now_ms: i64) -> Vec<BackendEvent> {
    let timer = (set.secs > 0).then(|| Duration::from_secs(u64::from(set.secs)));
    let mut patch = ConversationPatch::new(set.conv.clone());
    patch.timer = timer.map_or(Patch::Clear, Patch::Set);
    patch.timer_version = u32::try_from(at_s).ok().filter(|version| *version > 0);
    let notice = IncomingMessage::new(
        set.conv.clone(),
        set.id.local.clone(),
        Origin::OwnThisDevice,
        Body::System(SystemEvent::TimerChanged { by: None, timer }),
        set.id.sent_at,
        now_ms,
    );
    vec![
        BackendEvent::ConversationUpsert(patch),
        BackendEvent::Message(notice),
    ]
}

/// Runs the change: the order in the module doc. `Ok` once the server acked the frame AND the confirmation is durable.
pub(crate) async fn set(
    parts: Parts,
    writer: EventWriter,
    change: Set,
) -> Result<(), BackendError> {
    let looked = {
        let conv = change.conv.clone();
        parts
            .store
            .read(move |conn| Ok((started::holds(conn, &conv)?, tokens::facts(conn, &conv)?)))
            .await
    };
    let (started, token_facts) = match looked {
        Ok(looked) => looked,
        Err(e) => {
            tracing::warn!(kind = WHAT, error = %e, "a timer change could not read its store");
            return Err(BackendError::transient("the store could not be read"));
        }
    };
    // 2. D6 in the backend.
    if !started {
        tracing::info!(
            kind = WHAT,
            "a timer change in a chat the phone has not started was refused"
        );
        return Err(BackendError::unsupported(START_ON_PHONE));
    }
    // 3. The hold.
    if let Some(held) = held_back(WHAT, parts.hold.gate()) {
        return Err(held);
    }
    // 4. The token gate.
    if tokens::decide(true, token_facts, parts.linked_ms, parts.now_ms) {
        tracing::info!(
            kind = WHAT,
            "a timer change waits for the chat's token from the history"
        );
        return Err(BackendError::new(
            ErrorKind::Transient {
                retry_after: Some(TOKEN_RETRY),
            },
            "the chat's history has not arrived yet",
        ));
    }
    // 5. Our own ids and a session.
    if parts.own.pn.is_none() || parts.own.lid.is_none() {
        return Err(BackendError::transient(
            "the account's own ids are not known yet",
        ));
    }
    let Some(session) = parts.session.clone() else {
        return Err(error_of(Cause::Failure(SendFailure::NoClient)));
    };
    // 6. The message, and the hold again.
    let Ok(to) = jid_of(&change.conv) else {
        return Err(BackendError::unsupported(
            "this chat has no address on WhatsApp",
        ));
    };
    let at_s = parts.now_ms.div_euclid(1000);
    let out = Outgoing {
        to,
        id: change.id.local.to_string(),
        message: content::timer_setting(change.secs, at_s),
        expiration: None,
        devices: Devices::Cached,
        groups: Groups::Cached,
    };
    if let Some(held) = held_back(WHAT, parts.hold.gate()) {
        return Err(held);
    }
    // 7. The point of no return: only the app's quit can have come first.
    if !parts.stop.stop().close() {
        tracing::info!(
            kind = WHAT,
            "a timer change was ended by the quit before its frame"
        );
        return Err(stop::quitting());
    }
    // 8. The registry, then the frame.
    let (waited, left) = match write_once(&*session, &parts.acks, out, "timer").await {
        Ok(Written::Waited(waited, left)) => (waited, left),
        Ok(Written::TimedOut(wait)) => {
            end_unanswered(&wait, &parts.acks, &*session, WHAT).await;
            return Err(no_ack());
        }
        Err(error) => return Err(error),
    };
    // 9. The answer.
    let Waited::Answered(answer) = waited else {
        return Err(connection_ended());
    };
    match answer.recorded() {
        Some(Outcome::Acked { .. }) => {
            confirm(writer, confirmation(&change, at_s, parts.now_ms)).await
        }
        Some(Outcome::Refused) => Err(refused()),
        None => {
            let Answer::Nacked(nack) = answer else {
                unreachable!("an ack is always recorded")
            };
            Err(nack_error(nack, left.or_else(|| nack.floor())))
        }
    }
}

/// Writes the confirmation in one zone of the event writer (off the actor's runtime: the call parks its thread until the
/// commit) and returns after it. A failure leaves the server with the new timer and the core with the old one.
async fn confirm(writer: EventWriter, events: Vec<BackendEvent>) -> Result<(), BackendError> {
    match tokio::task::spawn_blocking(move || writer.write_and_wait(&events)).await {
        Ok(Ok(())) => Ok(()),
        _ => {
            tracing::error!(
                kind = WHAT,
                "a timer change was acked but its confirmation could not be written"
            );
            Err(BackendError::internal(
                "the timer was set on WhatsApp but could not be recorded here",
            ))
        }
    }
}

#[cfg(test)]
mod tests;
