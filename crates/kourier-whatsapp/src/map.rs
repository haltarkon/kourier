//! The map: one inbox row -> the core events it stands for (architecture §7.1, stage 2). W3a made it
//! (a text, and an unsupported message for the rest); W3b makes it real, chunk by chunk, and raises
//! [`MAP_VERSION`] so every row W3a deferred is taken up again.
//!
//! PURE: no I/O, no clock, no logging. [`class::class_of`] says what the stored message is; a row maps
//! to a LIST of events (one `core_outbox` row, in order: a merge of two ids, the chat's upsert, then the
//! message or the modifier). A text becomes a `BackendEvent::Message`; content this build does not
//! draw yet becomes a message with `Body::Unsupported` that keeps the encoded `wa::Message` in `raw`.
//!
//! Three outcomes besides an event (W3a fix F3; nothing this build cannot map is lost):
//! - [`Mapped::inert`]: the item is PROVABLY NOT A MESSAGE ([`class::Class::Inert`], the one list: a
//!   sender-key distribution with no other content at all, the app-state key shares and requests, a
//!   peer's history-sync notice, the peer-data-operation request and response) or it is a STATUS update
//!   (`status@broadcast`: the user's word "let them go", D1); the row is finished and its content
//!   wiped, no event.
//! - [`Mapped::deferred`]: a kind this build does not map yet (a poll vote, an encrypted reaction, an
//!   album parent, a pin, a protocol message that is not on the inert list, a channel's item, our own
//!   message to a broadcast list, a chat or sender of a server kind it does not know: `@hosted`,
//!   `@bot`, `@interop`, `@msgr`, ...: `@hosted` and `@hosted.lid` are NOT among them since the W3b fix F5, they read as `pn:` and `lid:`); stage 2 keeps the row WITH its content under the state
//!   `deferred`, which the poison rule never looks at, and a later build whose [`MAP_VERSION`] is
//!   higher takes it up again.
//! - a [`Failure`]: the row itself is broken (it does not decode, an id does not parse); the poison
//!   rule counts it.
//!
//! And two answers of W6a2 (D11, M3) for a revoke in a GROUP that is not the sender's own message: [`Mapped::needs_facts`]
//! (the map is pure and cannot read the store: the caller reads [`GroupFacts`] for those rows and maps them again with
//! [`map_row_with`]) and [`Mapped::waits_for_members`] (an admin's delete the core can only judge from the member list it was
//! told: stage 2 parks the row until the zone that states the group's members lets it go).
//!
//! Ids follow the contract's WhatsApp grammar: a person is `lid:<user>` or `pn:<user>`, a group
//! `wag:<user>`; a phone-number chat and a LID chat are two conversations until a stanza vouches for
//! the pair (the map then states `IdentityMerged` first).
//!
//! A failure carries a log-safe reason (a `&'static str`, never an input): the caller counts it
//! towards the poison rule.
//!
//! The disappearing timer of a message is, in this order: the one upstream handed over with the
//! message (kept in the row's meta), the one the stored message itself carries (a redelivery replay
//! re-parses the stanza and loses the first, not the message). A message that carries none has NO
//! expiry: there is no chat-level fallback (fix F1: a chat whose timer was switched off sends
//! messages with none, and a remembered timer erased them).

use std::fmt;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::{
    BackendBlob, Body, ConvKey, DisappearKind, Expiry, Origin as Sender, Placeholder, UserId,
};
use wacore::proto_helpers::MessageExt as _;
use wacore_binary::{Jid, JidExt as _, Server};
use waproto::whatsapp as wa;

use crate::inbox::{InboxMeta, Origin};

mod body;
pub(crate) mod class;
mod context;
pub(crate) mod facts;
mod modifier;

use class::{Class, Modifier};
pub(crate) use facts::GroupFacts;

/// Version of the payload `raw` carries: an encoded `wa::Message`. An unsupported message keeps the whole
/// stored message; a mapped one keeps only what a later step needs of the outer `messageContextInfo`
/// (`body::kept_context`: an album child's association, a poll's secret), so a reader decodes it the same way.
const RAW_VERSION: u16 = 1;

/// The version of THIS map. A row deferred by an older version is taken up again by stage 2 at the
/// start of an actor of a build whose version is higher (`inbox.map_version`). Raise it WHENEVER a
/// kind leaves the deferred list or a rule changes what a deferred row becomes (W3b made it 2: the
/// revokes, edits and reactions, the statuses and the broadcast lists W3a deferred; W4 made it 3: the
/// phone's history-sync notices, which W3b deferred, are now recorded as announced chunks; W6a2 made it 4: a group's
/// revoke of somebody else's message, which W3b deferred, is an admin's delete the core can judge, and three protocol
/// kinds W3b deferred for W6 are finished with no event, D13).
pub(crate) const MAP_VERSION: u32 = 4;

/// One `inbox` row, as stage 2 reads it. `Debug` prints no content.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) arrival: i64,
    pub(crate) chat: String,
    pub(crate) sender: String,
    pub(crate) id: String,
    pub(crate) origin: Origin,
    pub(crate) received_at: i64,
    pub(crate) catch_up: bool,
    pub(crate) message: Vec<u8>,
    pub(crate) meta: Vec<u8>,
    pub(crate) attempts: u32,
    /// The wall clock (ms) of the last take-up of the row by a newer map (0: never). The poison rule's
    /// age counts from the later of this and `received_at` (fix F12).
    pub(crate) aged_from: i64,
}

impl fmt::Debug for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Row")
            .field("arrival", &self.arrival)
            .field("origin", &self.origin)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

/// What a row maps to.
#[derive(Debug)]
pub(crate) struct Mapped {
    /// The events of the row, in the order they go into ONE `core_outbox` row: `IdentityMerged`, the
    /// chat's `ConversationUpsert`, then the message or the modifier. Empty for an item that is no
    /// message and for a deferred one.
    pub(crate) events: Vec<BackendEvent>,
    /// This build cannot map the item: the row stays, with its content (see the module doc).
    pub(crate) deferred: bool,
    /// The id of a history-sync notice from OUR OWN phone: a chunk of the message history is on its way.
    /// Stage 2 records it in the zone that finishes the row (W4, `history_announced`), so a chunk that
    /// is announced and never captured is counted lost at the next start. No event goes with it.
    pub(crate) announced: Option<String>,
    /// A revoke in a group that only the store's facts can judge ([`GroupFacts`]) and the caller gave none: nothing else
    /// is meant by this row, the caller reads the facts and maps the row again (W6a2, B7a).
    pub(crate) needs_facts: bool,
    /// An ADMIN's delete-for-everyone in this group (W6a2, D11): the core judges it by the member list it was told and drops
    /// it for good without one, so the row WAITS (stage 2 parks it, without a poison attempt) until the zone that states
    /// the group's members lets it go.
    pub(crate) waits_for_members: Option<ConvKey>,
    /// The name the SENDER of this message calls himself (W6a2, B8a, D8): cleaned ([`crate::names::clean`]) and only for a
    /// message of somebody else that made events. The map is pure; the zone compares it with what the core was told and
    /// states it on a change ([`crate::names::state_push_name`]).
    pub(crate) push_name: Option<crate::names::Named>,
}

impl Mapped {
    /// An item that is provably no message: finished, no event.
    pub(crate) fn inert() -> Self {
        Self {
            events: Vec::new(),
            deferred: false,
            announced: None,
            needs_facts: false,
            waits_for_members: None,
            push_name: None,
        }
    }

    /// A group's revoke that needs the store's facts to be judged.
    pub(crate) fn needing_facts() -> Self {
        Self {
            needs_facts: true,
            ..Self::inert()
        }
    }

    /// An admin's delete that waits for the member list of `group`.
    pub(crate) fn waiting_for_members(group: ConvKey) -> Self {
        Self {
            waits_for_members: Some(group),
            ..Self::inert()
        }
    }

    /// A history-sync notice of our own phone: finished like an inert item, and its id recorded.
    pub(crate) fn announced(id: &str) -> Self {
        Self {
            announced: Some(id.to_owned()),
            ..Self::inert()
        }
    }

    /// An item of a kind a later build maps: kept.
    pub(crate) fn deferred() -> Self {
        Self {
            deferred: true,
            ..Self::inert()
        }
    }
}

/// Our own ids: they decide which mention is ours and whose message a key names. Stage 2 reads them from
/// the device row; a device that is not paired has none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OwnIds {
    pub(crate) lid: Option<UserId>,
    pub(crate) pn: Option<UserId>,
}

impl OwnIds {
    /// Whether `user` is one of our own ids.
    pub(crate) fn is_own(&self, user: &UserId) -> bool {
        self.lid.as_ref() == Some(user) || self.pn.as_ref() == Some(user)
    }
}

/// What the map needs besides the row: stage 2 builds it once per step.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ctx {
    pub(crate) own: OwnIds,
}

/// Why a row could not be mapped. Log-safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Failure(pub(crate) &'static str);

/// Where a chat id leads.
enum Chat {
    Conv(ConvKey),
    /// The status feed (`status@broadcast`): let go on arrival (D1, the user's "Let them go").
    Status,
    /// A broadcast LIST: an incoming message of one shows up in the direct chat with its sender (as
    /// whatsmeow's `IsIncomingBroadcast` says, critic H3); our own message to a list has no
    /// conversation in the contract and stays deferred.
    List,
}

/// Why a chat or sender id gives nothing.
enum Miss {
    /// The id is broken: a failure of the row.
    Broken(Failure),
    /// The id is of a server kind this build does not map: the row is deferred, not failed.
    Unmapped,
}

impl From<Failure> for Miss {
    fn from(failure: Failure) -> Self {
        Self::Broken(failure)
    }
}

fn fail(reason: &'static str) -> Failure {
    Failure(reason)
}

fn user(scheme: &str, jid: &Jid) -> Result<UserId, Failure> {
    UserId::from_parts(scheme, &jid.user).map_err(|_| fail("a user id is not valid"))
}

/// The contract's scheme of a server kind: `pn` for a phone-number server, `lid` for a LID server, and
/// the HOSTED namespaces are read as their ordinary one (`@hosted` is `pn:`, `@hosted.lid` is `lid:`):
/// they are other devices of the same identity, upstream lists `(pn, Server::Hosted)` and `(lid,
/// Server::HostedLid)` as namespaces of one person, and whatsmeow rewrites them before anything else
/// (W3b fix F5). The ONE place that says it: `person_of`, `conv_of`, `chat_of`, `sender_of` and the pair
/// rules of `context`, `notice` and `calls` all follow it. `None` for every other kind of server.
pub(crate) fn scheme_of(server: Server) -> Option<&'static str> {
    if server.is_pn_family() {
        Some("pn")
    } else if server.is_lid_family() {
        Some("lid")
    } else {
        None
    }
}

/// The conversation a chat id stands for: a person (`pn:` or `lid:`) or a group (`wag:`). `None` for
/// every other kind of id (broadcast, newsletter, ...) and for a user part the contract refuses.
/// The receipts of the handler (`translate::durable`) use it too, so both ends of the crate speak
/// one grammar.
pub(crate) fn conv_of(jid: &Jid) -> Option<ConvKey> {
    if jid.server == Server::Group {
        return Some(ConvKey::Group(format!("wag:{}", jid.user).into()));
    }
    person_of(jid).map(ConvKey::Direct)
}

/// The person an id stands for (`pn:` or `lid:`, hosted devices included: [`scheme_of`]); `None` for a
/// group, a device-less server id and everything else.
pub(crate) fn person_of(jid: &Jid) -> Option<UserId> {
    user(scheme_of(jid.server)?, jid).ok()
}

/// `(number, lid)` of two ids a stanza vouches for as ONE person: one on a number server, one on a LID
/// server (the hosted namespaces count as their ordinary ones: [`scheme_of`]), neither of them ours (the
/// core skips those too). The ONE pair rule: the messages' `sender_alt` (`context::merges`), a group
/// notification's participants (`notice`) and a receipt's actor (`translate::receipts`) all use it.
pub(crate) fn vouched_pair(a: &Jid, b: &Jid, own: &OwnIds) -> Option<(UserId, UserId)> {
    let (pn, lid) = match (scheme_of(a.server)?, scheme_of(b.server)?) {
        ("pn", "lid") => (a, b),
        ("lid", "pn") => (b, a),
        _ => return None,
    };
    let (pn, lid) = (person_of(pn)?, person_of(lid)?);
    (!own.is_own(&pn) && !own.is_own(&lid)).then_some((pn, lid))
}

fn chat_of(text: &str) -> Result<Chat, Miss> {
    let jid: Jid = text
        .parse()
        .map_err(|_| Miss::Broken(fail("the chat id does not parse")))?;
    Ok(match jid.server {
        server if server == Server::Group || scheme_of(server).is_some() => {
            Chat::Conv(conv_of(&jid).ok_or_else(|| Miss::Broken(fail("a user id is not valid")))?)
        }
        Server::Broadcast if jid.is_status_broadcast() => Chat::Status,
        Server::Broadcast => Chat::List,
        _ => return Err(Miss::Unmapped),
    })
}

/// The sender and its device.
fn sender_of(text: &str) -> Result<(UserId, Option<u32>), Miss> {
    let jid: Jid = text
        .parse()
        .map_err(|_| Miss::Broken(fail("the sender id does not parse")))?;
    if scheme_of(jid.server).is_none() {
        return Err(Miss::Unmapped);
    }
    let author = person_of(&jid).ok_or_else(|| Miss::Broken(fail("a user id is not valid")))?;
    Ok((author, (jid.device > 0).then_some(u32::from(jid.device))))
}

/// Maps `row` with none of the store's facts: a group's revoke that needs them comes out as [`Mapped::needs_facts`].
pub(crate) fn map_row(row: &Row, ctx: &Ctx) -> Result<Mapped, Failure> {
    map_row_with(row, ctx, None)
}

/// Maps `row`; `facts` are the store's facts about the step's group revokes (`None`: not read yet).
pub(crate) fn map_row_with(
    row: &Row,
    ctx: &Ctx,
    facts: Option<&GroupFacts>,
) -> Result<Mapped, Failure> {
    let meta: InboxMeta = serde_json::from_slice(&row.meta)
        .map_err(|_| fail("the row's metadata does not decode"))?;
    // Channels are not shown before K8b: the item is kept.
    if row.origin == Origin::Newsletter {
        return Ok(Mapped::deferred());
    }
    let message = waproto::codec::message_decode(&row.message)
        .map_err(|_| fail("the row's message does not decode"))?;
    let base = class::peel(&message);
    let class = class::class_of(base);
    if class == Class::Inert {
        return Ok(Mapped::inert());
    }
    // The phone says a chunk of the message history is coming. Upstream honours only a notice from our
    // own phone (`from_me`), so only that one is announced: a peer's is plumbing nobody acts on (W4, H1).
    if class == Class::HistoryNotice {
        return Ok(if meta.from_me {
            Mapped::announced(&row.id)
        } else {
            Mapped::inert()
        });
    }
    let chat = chat_of(&row.chat);
    // A status update is let go whatever it holds, a deferred kind included (D1).
    if matches!(chat, Ok(Chat::Status)) {
        return Ok(Mapped::inert());
    }
    // A kind a later step maps, or the companion upload of another message (fix F7).
    if let Class::Deferred(_) = class {
        return Ok(Mapped::deferred());
    }
    if class::is_companion(&message, base) {
        return Ok(Mapped::deferred());
    }
    let sender = sender_of(&row.sender);
    let conv = match chat {
        Ok(Chat::Conv(conv)) => conv,
        // A broadcast list: an incoming message is in the direct chat with its sender; our own has
        // no conversation yet.
        Ok(Chat::List) if !meta.from_me => match &sender {
            Ok((author, _)) => ConvKey::Direct(author.clone()),
            Err(Miss::Unmapped) => return Ok(Mapped::deferred()),
            Err(Miss::Broken(failure)) => return Err(*failure),
        },
        Ok(Chat::List | Chat::Status) | Err(Miss::Unmapped) => return Ok(Mapped::deferred()),
        Err(Miss::Broken(failure)) => return Err(failure),
    };
    let (author, device) = match sender {
        Ok(who) => who,
        Err(Miss::Unmapped) => return Ok(Mapped::deferred()),
        Err(Miss::Broken(failure)) => return Err(failure),
    };
    let named = (!meta.from_me && !ctx.own.is_own(&author))
        .then(|| {
            crate::names::clean(&meta.push_name).map(|name| crate::names::Named {
                user: author.clone(),
                name,
            })
        })
        .flatten();
    let mut mapped = message_events(
        Input {
            conv,
            author,
            device,
            from_me: meta.from_me,
            id: &row.id,
            server_ms: meta.timestamp.saturating_mul(1000),
            received_at: row.received_at,
            message: &message,
            base,
            class,
            stored: Some(&row.message),
            carried: meta.ephemeral_expiration,
            arrival: Some(u64::try_from(row.arrival).unwrap_or(0)),
            catch_up: row.catch_up,
            merges: context::merges(row, &meta, ctx),
            facts,
        },
        ctx,
    )?;
    // Only a message that made events names its sender: a key share or a deferred kind is no sign of a person.
    if !mapped.events.is_empty() {
        mapped.push_name = named;
    }
    Ok(mapped)
}

/// One decoded message, where it belongs and who sent it, whichever way it came: the inbox row of the live
/// path ([`map_row`]) or a message of a history chunk (W4, `history::map`). The caller has already told what
/// the message IS (`class` is the class of the peeled `base`: [`Class::Text`], `Content`, `Unsupported` or a
/// `Modifier`; an inert or deferred kind, a status and a companion upload never get here) and has resolved the
/// conversation and the sender.
pub(crate) struct Input<'a> {
    pub(crate) conv: ConvKey,
    pub(crate) author: UserId,
    pub(crate) device: Option<u32>,
    pub(crate) from_me: bool,
    /// The message's id in its conversation (the stanza's, or the history message key's).
    pub(crate) id: &'a str,
    /// The server's time (ms): the stanza's `t`, or the history message's timestamp.
    pub(crate) server_ms: i64,
    pub(crate) received_at: i64,
    /// The whole decoded message and the same one with its wrappers peeled ([`class::peel`]).
    pub(crate) message: &'a wa::Message,
    pub(crate) base: &'a wa::Message,
    pub(crate) class: Class,
    /// The message as stored (encoded), kept in `raw` when it is shown as unsupported; `None`: it is encoded
    /// here when that is needed.
    pub(crate) stored: Option<&'a [u8]>,
    /// The disappearing timer handed over with the message; the message's own comes next.
    pub(crate) carried: Option<u32>,
    /// The inbox arrival counter (the live path's), `None` for a history message.
    pub(crate) arrival: Option<u64>,
    pub(crate) catch_up: bool,
    /// The `IdentityMerged` events the message's source vouches for, which go first.
    pub(crate) merges: Vec<BackendEvent>,
    /// The store's facts about a group's revoke (W6a2, D11, M3); `None`: not read. A history message has none: the
    /// batch cannot carry a modifier.
    pub(crate) facts: Option<&'a GroupFacts>,
}

/// The events of one decoded message ([`Input`]): the merges, the chat's upsert when the message sets the
/// chat's timer, then the message or the modifier. Pure.
pub(crate) fn message_events(input: Input<'_>, ctx: &Ctx) -> Result<Mapped, Failure> {
    let Input {
        conv,
        author,
        device,
        from_me,
        id,
        server_ms,
        received_at,
        message,
        base,
        class,
        stored,
        carried,
        arrival,
        catch_up,
        merges,
        facts,
    } = input;
    let author_id = author.clone();
    let origin = if from_me {
        Sender::OwnOtherDevice { device }
    } else {
        Sender::Remote { author, device }
    };

    let who = modifier::Who {
        conv: &conv,
        sender: &author_id,
        from_me,
        at: server_ms,
        id,
        ctx,
        facts,
    };
    // A reaction, an edit and a revoke: one event on their target. One this build cannot aim stays; an admin's
    // delete whose group has no member list yet waits (W6a2, D11).
    if let Class::Modifier(kind) = class
        && kind != Modifier::TimerSetting
    {
        return match modifier::events(kind, base, &who) {
            modifier::Modified::Events(modified) => {
                let mut events = merges;
                events.extend(modified);
                valid(events)
            }
            modifier::Modified::Keep => Ok(Mapped::deferred()),
            modifier::Modified::NeedsFacts => Ok(Mapped::needing_facts()),
            modifier::Modified::WaitsForMembers(group) => Ok(Mapped::waiting_for_members(group)),
        };
    }

    // The chat's timer setting: the chat's upsert and, in a 1:1 chat, the notice (a group's change is
    // announced by the group notice, so one change is one notice).
    let setting = (class == Class::Modifier(Modifier::TimerSetting))
        .then(|| modifier::timer_setting(base, &conv))
        .flatten();
    let info = context::info_of(base);
    let carried = carried.or_else(|| base.get_ephemeral_expiration());
    let (body, raw, mentions_me) = if let Some((_, timer)) = &setting {
        (modifier::timer_notice(&who, *timer), None, false)
    } else {
        match body::shown(message, base, class, info, ctx) {
            Some(shown) => (
                shown.body,
                shown.keep.map(|kept| BackendBlob::new(RAW_VERSION, kept)),
                shown.mentions_me,
            ),
            None => (
                Body::Unsupported {
                    kind_hint: match class {
                        Class::Content(content) => content.hint(),
                        Class::Unsupported(kind) => kind,
                        _ => "message",
                    }
                    .to_owned(),
                    summary: None,
                },
                Some(BackendBlob::new(
                    RAW_VERSION,
                    stored.map_or_else(|| waproto::codec::message_to_vec(message), <[u8]>::to_vec),
                )),
                false,
            ),
        }
    };
    let reply_to = if setting.is_some() {
        None
    } else {
        context::reply_to(&conv, info, ctx)
    };
    let announced_elsewhere = setting.is_some() && matches!(conv, ConvKey::Group(_));
    let mut events = merges;
    if let Some((patch, _)) = &setting {
        events.push(BackendEvent::ConversationUpsert(patch.clone()));
    }
    if !announced_elsewhere {
        let mut event = IncomingMessage::new(conv, id, origin, body, server_ms, received_at);
        event.reply_to = reply_to;
        // The core measures the edit and delete windows from the server's time, which is the stanza's `t`.
        event.server_at = Some(server_ms);
        event.mentions_me = mentions_me;
        event.arrival = arrival;
        event.catch_up = catch_up;
        event.view_once = message.is_view_once();
        event.raw = raw;
        // A message carries only its own timer; the notice of a timer change carries none.
        event.expiry = carried
            .filter(|seconds| *seconds > 0 && setting.is_none())
            .map(|seconds| Expiry {
                kind: DisappearKind::AfterSend,
                timer: Duration::from_secs(u64::from(seconds)),
                started_at: None,
            });
        events.push(BackendEvent::Message(event));
    }
    valid(events)
}

/// The row's events, once every id passes the contract's own check.
fn valid(events: Vec<BackendEvent>) -> Result<Mapped, Failure> {
    for event in &events {
        event
            .validate_ids()
            .map_err(|_| fail("an id is not valid for the contract"))?;
    }
    Ok(Mapped {
        events,
        ..Mapped::inert()
    })
}

/// What a message that could not be mapped for good leaves in the timeline: Signal's rule, a
/// placeholder in the sender's conversation. `None` for an own message, an unreadable row or a chat
/// that has no conversation.
pub(crate) fn poison_placeholder(row: &Row) -> Option<BackendEvent> {
    let meta: InboxMeta = serde_json::from_slice(&row.meta).ok()?;
    if meta.from_me {
        return None;
    }
    let Ok(Chat::Conv(conv)) = chat_of(&row.chat) else {
        return None;
    };
    let (author, device) = sender_of(&row.sender).ok()?;
    let mut event = IncomingMessage::new(
        conv,
        row.id.as_str(),
        Sender::Remote { author, device },
        Body::Placeholder(Placeholder::Undecryptable { resendable: false }),
        meta.timestamp.saturating_mul(1000),
        row.received_at,
    );
    event.arrival = Some(u64::try_from(row.arrival).unwrap_or(0));
    event.catch_up = row.catch_up;
    let event = BackendEvent::Message(event);
    event.validate_ids().ok()?;
    Some(event)
}

#[cfg(test)]
mod tests;
