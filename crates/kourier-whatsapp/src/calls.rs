//! Calls: shown, never taken (W3b, chunk C7; brief 3.9; decisions D2 and D3 of `w3b-decisions.md`).
//!
//! NOTHING IS EVER SENT FOR A CALL. A reply from one of the user's devices can end the call or stop the
//! phone ringing, so this backend never answers one: it takes the stanza away from upstream's own call
//! handler (which would send an offer-ack `<receipt>`, learn identities and, with the media features,
//! start a call) and keeps for itself the one thing the server is owed, the transport `<ack>`, which
//! upstream still sends for a claimed `<call>` (`node_io.rs` `dispatch_node_scoped`: a claim of a
//! `<receipt>`, `<notification>` or `<call>` leaves the ack where it was).
//!
//! HOW. [`CallInterceptor`] is registered once per client, right after the build and BEFORE the client
//! starts (`session::register_stanzas`: ONE interceptor with the pairing's claim of `prepair`, so the offline
//! queue's calls are claimed too), and its handle lives
//! in the running client for as long as the client does: dropping the handle un-registers the
//! interceptor and the next offer would be answered by upstream. It looks at the tag `call` and nothing
//! else (any other tag is `Pass`); a `<call>` is parsed by upstream's own parser, mapped to AT MOST ONE
//! call line ([`line`], the table below), written with the group commit and WAITED for (rule (a): upstream
//! acknowledges the claimed stanza right after the interceptor returns, so the line is durable before the
//! server is told), and claimed. EVERY failure still claims the stanza: a stanza that does not parse, an
//! action this version does not map, a write that failed or was held. A failed write ends the connection
//! first (`signal_shutdown_sync` suppresses this ack and every later one; the server sends the stanza
//! again, as for the handler's own durable writes).
//!
//! Where it runs. A `<call>` is processed on a task of its own on the client's runtime, never on the read
//! loop (`processes_inline` has no call arm), so a commit parks one of the account's two workers, like a
//! receipt's. An offer and its terminate can park both at once beside a receipt's handler; each wait is
//! bounded (the link hold's 10 s), so the third waiter waits for a free worker, never for ever.
//!
//! WHAT A CALL IS HERE (the contract's table, `kourier_backend_api::IncomingMessage::call`):
//!
//! | `<call>` | origin | line |
//! |---|---|---|
//! | `offer`, live, no `group-jid` | `Remote` (the caller) | incoming 1:1, ringing for [`RING_MS`], voice or video, by the caller |
//! | `offer` with `offline` | `Remote` | the same with no time left (a missed call at once), `catch_up` |
//! | `offer` with `group-jid` | `Remote` (the ringer) | incoming group call, ringing, in `wag:<group>` |
//! | `terminate`, no reason, `timeout` or `group_call_ended`, no duration | `Remote` | incoming, missed |
//! | `terminate` with `accepted_elsewhere` or `rejected_elsewhere` | | NOTHING (D2) |
//! | `offer_notice`, `preaccept`, `accept`, `reject`, `transport`, relay latency, video, group update, rekey, waiting room, hand, screen share, an action this version does not know | | nothing (still claimed) |
//!
//! D2 (his words: "missed until the phone says"): a terminate that says `accepted_elsewhere` or
//! `rejected_elsewhere` is the CALLER's word, sent by the caller's side to the callee's other devices
//! (upstream: "the dismiss is caller-driven", `handlers/call.rs`), so it is not the user's own device
//! speaking and is never labelled `OwnOtherDevice`. It reports nothing: the ring ends as a missed call at
//! its deadline, until the phone's call log (W6a1, `calls/log.rs`) says what happened. A terminate of a call WE made
//! (`call-creator` is one of our ids) is not an incoming call and reports nothing either; so does any
//! signal of a call made by one of our own ids.
//!
//! The ring. A live offer rings for [`RING_MS`], the whole time: this computer's clock is not in it (a
//! wrong local clock neither shortens nor lengthens a ring, as Signal's), and a live offer is seconds old.
//! An `offline` offer is a replay from the server's queue: upstream's own handler shows it as a missed
//! call and does not ring it, so it has no time left here either.
//!
//! WHO MAY BE NAMED (W3b fix F1, privacy review HIGH 1). `call-creator` is a CLAIM written by the
//! sending device, and `from`/`participant` are the server's: upstream itself refuses a group terminate
//! whose creator is not the routed sender. A line is placed only for a creator who IS the sender
//! ([`creator_is_sender`]): the same user on the same kind of server as `participant` or `from` (the
//! device ignored; the hosted namespaces count as the ordinary ones), or the other id of the same
//! person: for a LID creator the number the store's `lid_pn_mapping` gives, for a number creator the
//! number the mapping gives for the sender's LID. Only those two ties exist: the offer's `caller_pn` is
//! written by the caller like `call-creator` itself, so it is not read (a stranger could name his own
//! number there). Anyone else gets NOTHING (the stanza is still claimed and acknowledged): a stranger
//! cannot put a contact's name on a ringing line or on a missed call, nor ring "in" a group the user
//! shares with the creator it names.
//!
//! The conversation. A 1:1 call's chat is the CALLER's person (`call-creator`, the same in every action
//! of one call whatever the addressing mode of the stanza's `from`), and a group call's the group the
//! offer names. A `terminate` carries no group: it is placed only when its `from` is a person (1:1) or a
//! group; a group call's terminate comes from the call itself (`<id>@call` with a `participant`) and
//! reports nothing, so a group ring ends at its deadline too.
//!
//! LOGS carry counts and kinds only: never a JID, a number, a name or a call id. A stanza that does not
//! parse logs that fact and nothing of the error (an upstream parse error can print the JID that
//! failed to parse).

use std::sync::Arc;

use kourier_backend_api::{BackendEvent, Clock, Envelope, IncomingMessage};
use kourier_model::{CallDirection, CallLine, CallMedia, CallState, ConvKey, Origin};
use wacore::stanza::call::{
    TERMINATE_REASON_GROUP_CALL_ENDED, TERMINATE_REASON_TIMEOUT, parse_call_stanza,
};
use wacore::types::call::{CallAction, IncomingCall};
use wacore_binary::{Jid, NodeRef, OwnedNodeRef, Server};
use whatsapp_rust::{Interception, StanzaInterceptor};

use crate::events::EventWriter;
use crate::map::{self, OwnIds};
use crate::stage2::Stage2;

pub(crate) mod log;

/// How long a live offer rings (ms). WhatsApp's own ring time was not read from a source; 60 s is
/// RingRTC's (Signal's) and the phone's call screen is the truth while it rings. The core caps any ring
/// at two minutes whatever it is told.
pub(crate) const RING_MS: i64 = 60_000;

/// The environment variable of the debug switch (see [`claim`]).
pub(crate) const BUILTIN_ENV: &str = "KOURIER_WA_CALL_BUILTIN";

/// Who answers a call stanza after this backend wrote its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Claim {
    /// This backend takes the stanza: upstream's call handler never sees it, nothing is sent for the
    /// call but the transport ack. Every release build, and every debug build unless asked.
    Calls,
    /// Debug only, for R2's test of what upstream's handler would do: the line is still written, then
    /// upstream's own handler runs and sends its offer-ack.
    LetBuiltInAnswer,
}

/// The claim of THIS build: [`Claim::LetBuiltInAnswer`] only in a debug build AND when the switch is
/// exactly `1`. The build kind is read HERE (`cfg!(debug_assertions)`), not handed in: no caller can pass
/// "debug" to a release build, so the one call site needs no check of its own (fix F10, privacy review
/// LOW 3). A release build cannot reach the switch whatever the environment says.
pub(crate) fn claim(env: Option<&str>) -> Claim {
    claim_for_build(cfg!(debug_assertions), env)
}

/// The rule of [`claim`] for a given build kind. Private: the unit tests pass both kinds through it,
/// production code goes through [`claim`] and cannot name the kind.
fn claim_for_build(debug_build: bool, env: Option<&str>) -> Claim {
    if debug_build && env == Some("1") {
        Claim::LetBuiltInAnswer
    } else {
        Claim::Calls
    }
}

/// What reading one `<call>` stanza gave.
#[derive(Debug)]
pub(crate) enum Read {
    /// The call line to write.
    Line(Box<BackendEvent>),
    /// Nothing to show (an action that is no call line, or one this version leaves out).
    Nothing,
    /// The stanza did not parse. Log-safe: nothing of the upstream error is kept.
    Unreadable,
}

/// What the store knows of who is who: the phone number it maps a LID's user part to, if it knows
/// one. Asked only when the stanza itself does not settle whether the creator is its sender.
pub(crate) type NumberOf<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Reads a `<call>` stanza into its line, if it has one.
pub(crate) fn read(node: &NodeRef<'_>, now_ms: u64, own: &OwnIds, number_of: NumberOf<'_>) -> Read {
    match parse_call_stanza(node) {
        Ok(Some(call)) => match line(&call, now_ms, own, number_of) {
            Some(event) => Read::Line(Box::new(event)),
            None => Read::Nothing,
        },
        Ok(None) => Read::Nothing,
        // The error is dropped on purpose: its text can name the JID that failed to parse.
        Err(_) => Read::Unreadable,
    }
}

/// The call line of one signal, as the message that carries it. `None` when the signal says nothing the
/// table above reports, or when its ids do not pass the contract (a hostile call id).
pub(crate) fn line(
    call: &IncomingCall,
    now_ms: u64,
    own: &OwnIds,
    number_of: NumberOf<'_>,
) -> Option<BackendEvent> {
    let creator = call.action.call_creator();
    let caller = map::person_of(creator)?;
    if own.is_own(&caller) {
        // A call our own phone made (or any signal of it): not an incoming call.
        return None;
    }
    // The creator is the sender or a stranger who names him (fix F1): nothing for a stranger.
    if !creator_is_sender(call, number_of) {
        return None;
    }
    let at = call.timestamp?.timestamp_millis();
    let (conv, group, state, media) = match &call.action {
        CallAction::Offer {
            group_jid,
            is_video,
            ..
        } => {
            let (conv, group) = match group_jid {
                Some(jid) => (
                    map::conv_of(jid).filter(|conv| matches!(conv, ConvKey::Group(_)))?,
                    true,
                ),
                None => (ConvKey::Direct(caller.clone()), false),
            };
            let left = if call.offline { 0 } else { RING_MS };
            let state = CallState::Ringing {
                deadline: at.saturating_add(left),
            };
            let media = (!group).then_some(if *is_video {
                CallMedia::Video
            } else {
                CallMedia::Voice
            });
            (conv, group, state, media)
        }
        CallAction::Terminate {
            reason, duration, ..
        } => {
            // A connected call was not missed; a reason other than the caller giving up (or the ring
            // running out) says what some device did (D2), which only that device may say.
            if duration.is_some_and(|d| d > 0) {
                return None;
            }
            match reason.as_deref() {
                None | Some(TERMINATE_REASON_TIMEOUT | TERMINATE_REASON_GROUP_CALL_ENDED) => {}
                Some(_) => return None,
            }
            let (conv, group) = placed(&call.from, own)?;
            let conv = match conv {
                Some(group_conv) => group_conv,
                None => ConvKey::Direct(caller.clone()),
            };
            (conv, group, CallState::Missed, None)
        }
        _ => return None,
    };
    let mut signal = CallLine::signal(CallDirection::Incoming, group, state, at);
    if let Some(media) = media {
        signal = signal.with_media(media, at);
    }
    let signal = signal.with_by(caller.clone(), at);
    let device = (creator.device > 0).then_some(u32::from(creator.device));
    let mut message = IncomingMessage::call(
        conv,
        call.action.call_id(),
        Origin::Remote {
            author: caller,
            device,
        },
        signal,
        at,
        i64::try_from(now_ms).unwrap_or(i64::MAX),
    );
    message.server_at = Some(at);
    message.catch_up = call.offline;
    // The contract's own check, whole: ids, the call line's rules and its `local`. A row the core
    // would refuse for good is never written (a hostile call id: empty, too long, a control character).
    let event = BackendEvent::Message(message);
    let (envelope, _waiter) = Envelope::durable(vec![event.clone()]);
    envelope.validate().ok()?;
    Some(event)
}

/// Whether the call's creator is the one who sent the stanza (fix F1; the module doc says which ids
/// count). The stanza alone settles the usual case (same user and server kind as `participant` or
/// `from`); the store is asked only for the other id of the same person. Nothing the caller writes into
/// the stanza ties a creator to the sender: not `call-creator` itself, not the offer's `caller_pn`.
fn creator_is_sender(call: &IncomingCall, number_of: NumberOf<'_>) -> bool {
    let creator = call.action.call_creator();
    let Some(scheme) = map::scheme_of(creator.server) else {
        return false;
    };
    // The sender: the participant the stanza routes for and the address it came from, persons only.
    let senders: Vec<(&'static str, &str)> = [call.participant.as_ref(), Some(&call.from)]
        .into_iter()
        .flatten()
        .filter_map(|jid| Some((map::scheme_of(jid.server)?, jid.user.as_str())))
        .collect();
    let creator_user = creator.user.as_str();
    if senders
        .iter()
        .any(|&(kind, user)| kind == scheme && user == creator_user)
    {
        return true;
    }
    if scheme == "lid" {
        // A LID that names itself the sender's number: only the store's mapping can say so. The offer's
        // `caller_pn` is written by the caller, so it vouches for nothing and is not read here.
        number_of(creator_user).is_some_and(|number| {
            senders
                .iter()
                .any(|&(kind, user)| kind == "pn" && user == number)
        })
    } else {
        // A number that names itself the sender's LID: only the store's mapping can say so.
        senders.iter().any(|&(kind, user)| {
            kind == "lid" && number_of(user).is_some_and(|number| number == creator_user)
        })
    }
}

/// Where a terminate's `from` puts its line: `Some((Some(group), true))` for a group, `Some((None,
/// false))` for a person that is not one of ours (the 1:1 chat is the caller's), `None` for anything
/// else (a group call's own address, a server, one of our own ids).
fn placed(from: &Jid, own: &OwnIds) -> Option<(Option<ConvKey>, bool)> {
    match from.server {
        Server::Group => {
            let conv = map::conv_of(from).filter(|conv| matches!(conv, ConvKey::Group(_)))?;
            Some((Some(conv), true))
        }
        _ => {
            let person = map::person_of(from)?;
            (!own.is_own(&person)).then_some((None, false))
        }
    }
}

/// Claims every `<call>` stanza, writes the line it stands for and lets nothing answer it.
pub(crate) struct CallInterceptor {
    writer: EventWriter,
    clock: Arc<dyn Clock>,
    /// Our own ids, as far as they are known (a call of our own phone shows nothing).
    own: Arc<Stage2>,
    /// A write the account cannot go on without failed: ends the connection (the caller's way to do it).
    on_failure: Arc<dyn Fn() + Send + Sync>,
    claim: Claim,
}

impl CallInterceptor {
    pub(crate) fn new(
        writer: EventWriter,
        clock: Arc<dyn Clock>,
        own: Arc<Stage2>,
        on_failure: Arc<dyn Fn() + Send + Sync>,
        claim: Claim,
    ) -> Self {
        Self {
            writer,
            clock,
            own,
            on_failure,
            claim,
        }
    }

    /// What the interceptor returns when nothing went wrong with the write. In the debug switch's start, an `offer`
    /// handed to upstream's handler says so in ONE fixed INFO line (T2 F7: the run tool counts them, "upstream
    /// answered N call offers in this start"; no id, no number, no chat): that handler sends an offer-ack and learns
    /// the caller's number-to-LID pairing from the caller's own claim.
    fn after(&self, offer: bool) -> Interception {
        match self.claim {
            Claim::Calls => Interception::Handled,
            Claim::LetBuiltInAnswer => {
                if offer {
                    tracing::info!(
                        "upstream's call handler is given a call offer (a debug test switch)"
                    );
                }
                Interception::Pass
            }
        }
    }
}

/// Whether a `<call>` stanza carries an `offer` (the only action upstream's handler acknowledges). A stanza that does
/// not parse is none.
fn is_an_offer(node: &NodeRef<'_>) -> bool {
    matches!(
        parse_call_stanza(node),
        Ok(Some(call)) if matches!(call.action, CallAction::Offer { .. })
    )
}

impl StanzaInterceptor for CallInterceptor {
    fn intercept(&self, node: &OwnedNodeRef) -> Interception {
        let node = node.get();
        if node.tag != "call" {
            return Interception::Pass;
        }
        // The store's mapping is read only when the stanza does not settle who its creator is; a read
        // that fails reads as "no mapping" (nothing is shown for a creator that is not proven).
        let store = self.writer.store().clone();
        let number_of = move |lid: &str| -> Option<String> {
            let lid = lid.to_owned();
            store
                .read_blocking(move |conn| {
                    crate::store::first(
                        conn,
                        "get lid mapping failed",
                        "SELECT phone FROM lid_pn_mapping WHERE lid = ?1",
                        [lid],
                        |row| row.get::<_, String>(0),
                    )
                })
                .ok()
                .flatten()
        };
        let read = read(node, self.clock.now_ms(), &self.own.own_known(), &number_of);
        match read {
            Read::Line(event) => {
                if self.writer.write_and_wait(&[*event]).is_err() {
                    // No ack for this stanza or any later one of the connection; the server sends
                    // it again. Claimed whatever the debug switch says: upstream's handler must not
                    // answer a call whose line is not durable.
                    tracing::error!(
                        "a call line could not be written; the connection is being ended"
                    );
                    (self.on_failure)();
                    return Interception::Handled;
                }
                tracing::debug!(lines = 1, "a call line is durable");
            }
            Read::Nothing => {
                tracing::debug!(lines = 0, "a call signal that shows nothing");
            }
            Read::Unreadable => {
                tracing::warn!("a call stanza could not be read");
            }
        }
        self.after(self.claim == Claim::LetBuiltInAnswer && is_an_offer(node))
    }
}

#[cfg(test)]
mod tests;
