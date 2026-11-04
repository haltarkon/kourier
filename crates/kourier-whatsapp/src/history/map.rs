//! The history map (W4, C9): one `WebMessageInfo` of a chunk -> what the core is given for it. PURE: no I/O,
//! no clock (the caller says what time it is), no logging.
//!
//! A message of a chunk takes the SAME road as a live one: [`crate::map::message_events`] turns the decoded
//! `wa::Message` into the contract's `IncomingMessage` (the content, the quote, the mentions, the message's
//! own timer), and this module adds what only a history row has. The ids follow the live path's grammar
//! (`map::conv_of`, `person_of`), so a message that arrives live and again in the history is ONE row of the
//! core: `(conversation, author, local)` is the same on both roads (a test pins it).
//!
//! What a history row says beyond a live message:
//! - `read: true` on EVERY row and `silent`: WhatsApp states a chat's unread COUNT only, never which message is
//!   unread, so the rows say "read or not known" and the chat states the count as `unread_hint` (the core's
//!   rule: never both for the same messages; C10).
//! - `sent_at`/`server_at` = `messageTimestamp`, `received_at` = now, no arrival counter, not a catch-up.
//! - an OWN message (`key.fromMe`) says its state from `status` (`initial_out_state`; there is no "played"
//!   state, PLAYED reads as viewed; an ERROR is NOT retryable: the phone owns that message, a "Retry" here
//!   would send an old message from Kourier). An own message that was not sent keeps its timer without a
//!   start, as the contract asks.
//! - the timer is the message's own (`ephemeralDuration`, else the one inside the message) and its start
//!   (`ephemeralStartTimestamp`) when the phone had started it; there is no chat-level fallback (F1).
//! - the reactions of the message (`WebMessageInfo.reactions`).
//!
//! Nothing this build cannot show is lost or invented: a stub (a group notice, a "waiting for this message"
//! placeholder: O4), a modifier kind (a reaction, an edit, a revoke, the chat's timer setting: the contract
//! has no way to say them in a batch), a kind the live map defers, a companion upload, a message of a chat
//! this build has no conversation for (a broadcast list, a channel, a bot) and a message that is broken
//! (no key, id or time, a sender that is no person) are [`Item::Deferred`]: the caller keeps the encoded
//! message (`history_deferred`, with `MAP_VERSION`) for a later build. A status update is let go, as live
//! (his word, D1), and so is what is provably no message (a key share, a notice, an empty entry).

use kourier_backend_api::{BackendEvent, HistoryReaction, IncomingMessage};
use kourier_model::{ConvKey, FailReason, OutgoingState, UserId};
use wa::web_message_info::{Status, StubType};
use wacore_binary::{Jid, JidExt as _, Server};
use waproto::whatsapp as wa;

use crate::map::class::{self, Class};
use crate::map::{Ctx, Input, conv_of, message_events, person_of, scheme_of};

/// Most reactions read from one message (a message has a handful; the bound keeps a hostile chunk small).
const MAX_REACTIONS: usize = 100;

/// Where a chat of a chunk leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A conversation of the contract: a person (`lid:` or `pn:`) or a group (`wag:`).
    Conv(ConvKey),
    /// The status feed (`status@broadcast`): let go, as live.
    Status,
    /// A chat this build has no conversation for (a broadcast list, a channel, a bot, an id of another
    /// server kind or one that does not parse): its messages are kept, not shown.
    Unmapped,
}

/// A chat of a chunk, placed. `jid` is the id the chat was placed BY (see [`Place::of`]); it is what a deferred
/// item stores, so a later build places the same chat again with [`Place::from_jid`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) jid: String,
    pub(crate) target: Target,
}

impl Place {
    /// The place of a chunk's `Conversation`: a person's chat is placed by its LID form wherever the chunk
    /// states one (`lidJid`, else an `id` that is a LID, else `pnJid`, else the `id`), because a LID is what the
    /// live path names that chat by once the phone knows it; two forms of one person that do meet are joined by
    /// the chunk's pairs (C10). `locked` is not read: a locked chat is delivered like any other (D1).
    pub(crate) fn of(conversation: &wa::Conversation) -> Self {
        Self::from_jid(placed_by(conversation))
    }

    /// The place of a chat id.
    pub(crate) fn from_jid(jid: &str) -> Self {
        Self {
            jid: jid.to_owned(),
            target: target_of(jid),
        }
    }
}

/// The id a chat is placed by: its LID form wherever the chunk states one, else its number, else its id.
fn placed_by(conversation: &wa::Conversation) -> &str {
    let id = conversation.id.as_str();
    if !is_person(id) {
        return id;
    }
    let forms = [
        conversation.lid_jid.as_deref(),
        Some(id),
        conversation.pn_jid.as_deref(),
    ];
    for scheme in ["lid", "pn"] {
        if let Some(form) = forms
            .into_iter()
            .flatten()
            .find(|form| is_person(form) && scheme_of_text(form) == Some(scheme))
        {
            return form;
        }
    }
    id
}

fn scheme_of_text(text: &str) -> Option<&'static str> {
    scheme_of(text.parse::<Jid>().ok()?.server)
}

/// Whether `text` is the id of a person this build can name (`pn:` or `lid:`).
fn is_person(text: &str) -> bool {
    text.parse::<Jid>()
        .ok()
        .is_some_and(|jid| person_of(&jid).is_some())
}

fn target_of(text: &str) -> Target {
    let Ok(jid) = text.parse::<Jid>() else {
        return Target::Unmapped;
    };
    if jid.is_status_broadcast() {
        return Target::Status;
    }
    if jid.server == Server::Group || scheme_of(jid.server).is_some() {
        return conv_of(&jid).map_or(Target::Unmapped, Target::Conv);
    }
    Target::Unmapped
}

/// What a message of a chunk becomes.
#[derive(Debug)]
pub(crate) enum Item {
    /// A message for the chat's `HistoryBatch`.
    Message(Box<IncomingMessage>),
    /// This build cannot show it: the caller keeps the encoded message for a later build. The reason is a
    /// short static word, log-safe.
    Deferred(&'static str),
    /// Provably nothing to show or keep: a status update, a key share, an empty entry.
    Skip,
}

/// Maps one message of `place`. `now_ms` is the local wall clock (the message's `received_at`).
pub(crate) fn message(place: &Place, info: &wa::WebMessageInfo, ctx: &Ctx, now_ms: i64) -> Item {
    match map_one(place, info, ctx, now_ms) {
        Ok(item) => item,
        Err(reason) => Item::Deferred(reason),
    }
}

fn map_one(
    place: &Place,
    info: &wa::WebMessageInfo,
    ctx: &Ctx,
    now_ms: i64,
) -> Result<Item, &'static str> {
    let conv = match &place.target {
        Target::Status => return Ok(Item::Skip),
        Target::Unmapped => return Err("chat"),
        Target::Conv(conv) => conv,
    };
    let key = info.key.as_option().ok_or("no key")?;
    let id = key
        .id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or("no id")?;
    let server_ms = info
        .message_timestamp
        .and_then(|seconds| i64::try_from(seconds).ok())
        .and_then(|seconds| seconds.checked_mul(1000))
        .ok_or("no time")?;
    // A stub is the phone's own notice (a group changed, a number changed, a message it could not read):
    // O4, kept for a later build. The live notices' ids cannot be derived from a stub's key, so mapping
    // both would show one change twice.
    if info
        .message_stub_type
        .is_some_and(|stub| stub != StubType::UNKNOWN)
    {
        return Err("stub");
    }
    let Some(message) = info.message.as_option() else {
        return Ok(Item::Skip);
    };
    let base = class::peel(message);
    let class = class::class_of(base);
    match class {
        Class::Inert | Class::HistoryNotice => return Ok(Item::Skip),
        Class::Deferred(_) => return Err("kind"),
        Class::Modifier(_) => return Err("modifier"),
        Class::Text | Class::Content(_) | Class::Unsupported(_) => {}
    }
    if class::is_companion(message, base) {
        return Err("companion");
    }

    let mut from_me = key.from_me == Some(true);
    let (author, device) = if from_me {
        let own = ctx.own.lid.clone().or_else(|| ctx.own.pn.clone());
        (own.ok_or("no own id")?, None)
    } else {
        sender_of(conv, key, info)?
    };
    // A message the key does not call ours but whose sender is one of our own ids is ours all the same
    // (the chat with ourselves, a participant that is us).
    from_me |= ctx.own.is_own(&author);

    let mapped = message_events(
        Input {
            conv: conv.clone(),
            author,
            device,
            from_me,
            id,
            server_ms,
            received_at: now_ms,
            message,
            base,
            class,
            stored: None,
            carried: info.ephemeral_duration.filter(|seconds| *seconds > 0),
            arrival: None,
            catch_up: false,
            merges: Vec::new(),
            facts: None,
        },
        ctx,
    )
    .map_err(|failure| failure.0)?;
    if mapped.deferred {
        return Err("kind");
    }
    let mut event = mapped
        .events
        .into_iter()
        .rev()
        .find_map(|event| match event {
            BackendEvent::Message(event) => Some(event),
            _ => None,
        })
        .ok_or("kind")?;

    event.read = true;
    event.silent = true;
    if from_me {
        event.initial_out_state = Some(out_state(info.status, server_ms));
    }
    let unsent = matches!(
        event.initial_out_state,
        Some(OutgoingState::Queued | OutgoingState::Sending { .. } | OutgoingState::Failed { .. })
    );
    if let Some(expiry) = event.expiry.as_mut() {
        expiry.started_at = if unsent {
            None
        } else {
            started_at(info.ephemeral_start_timestamp, now_ms)
        };
    }
    event.reactions = reactions(conv, info, ctx, server_ms);
    Ok(Item::Message(Box::new(event)))
}

/// The author of a message of somebody else: the chat's person in a 1:1 chat, the participant in a group.
fn sender_of(
    conv: &ConvKey,
    key: &wa::MessageKey,
    info: &wa::WebMessageInfo,
) -> Result<(UserId, Option<u32>), &'static str> {
    match conv {
        ConvKey::Direct(person) => Ok((person.clone(), None)),
        ConvKey::Group(_) => {
            let jid: Jid = key
                .participant
                .as_deref()
                .filter(|text| !text.is_empty())
                .or(info.participant.as_deref())
                .ok_or("no sender")?
                .parse()
                .map_err(|_| "no sender")?;
            let author = person_of(&jid).ok_or("no sender")?;
            Ok((author, (jid.device > 0).then_some(u32::from(jid.device))))
        }
        ConvKey::Broadcast(_) | ConvKey::Channel(_) => Err("chat"),
    }
}

/// The state of an own message from the phone's `status`. No `Played`: a played voice message reads as viewed.
/// An error is not retryable. A status the phone did not state, or one a later pin adds (the enum is
/// non-exhaustive), is a message the server has: it is in the phone's history.
fn out_state(status: Option<Status>, server_ms: i64) -> OutgoingState {
    match status {
        Some(Status::ERROR) => OutgoingState::Failed {
            reason: FailReason::Other("failed on the phone".to_owned()),
            retryable: false,
        },
        Some(Status::PENDING) => OutgoingState::Sending { progress: None },
        Some(Status::DELIVERY_ACK) => OutgoingState::Delivered,
        Some(Status::READ) => OutgoingState::Read,
        Some(Status::PLAYED) => OutgoingState::Viewed,
        None | Some(_) => OutgoingState::Sent {
            server_at: Some(server_ms),
        },
    }
}

/// When the phone started the message's timer (ms), never in the future: "one whose start would lie in the
/// future must replace it itself" (the contract), and the present is the nearest honest answer.
fn started_at(seconds: Option<u64>, now_ms: i64) -> Option<i64> {
    let ms = i64::try_from(seconds.filter(|s| *s > 0)?)
        .ok()?
        .checked_mul(1000)?;
    Some(ms.min(now_ms))
}

/// The reactions of a message. A removed reaction (no emoji) and one whose reactor is no person are left
/// out. The reactor of a reaction is read off the REACTION's key: `fromMe` is us, `participant` is the reactor
/// in a group, and in a 1:1 chat the other side is the chat's person.
fn reactions(
    conv: &ConvKey,
    info: &wa::WebMessageInfo,
    ctx: &Ctx,
    default_ms: i64,
) -> Vec<HistoryReaction> {
    info.reactions
        .iter()
        .filter_map(|reaction| {
            let emoji = reaction.text.as_deref().filter(|text| !text.is_empty())?;
            let key = reaction.key.as_option();
            let by = if key.is_some_and(|key| key.from_me == Some(true)) {
                None
            } else {
                let user = match key
                    .and_then(|key| key.participant.as_deref())
                    .filter(|text| !text.is_empty())
                {
                    Some(text) => person_of(&text.parse::<Jid>().ok()?)?,
                    None => match conv {
                        ConvKey::Direct(person) => person.clone(),
                        _ => return None,
                    },
                };
                (!ctx.own.is_own(&user)).then_some(user)
            };
            Some(HistoryReaction {
                by,
                emoji: emoji.to_owned(),
                at: reaction
                    .sender_timestamp_ms
                    .filter(|ms| *ms > 0)
                    .unwrap_or(default_ms),
            })
        })
        .take(MAX_REACTIONS)
        .collect()
}

#[cfg(test)]
mod tests;
