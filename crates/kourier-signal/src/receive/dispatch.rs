//! Stage-2 content dispatch: one inbox row → core events and backend side effects (§6.13;
//! `MessageReceiver.preload.ts` `#innerHandleContentMessage`, `#handleDataMessage`,
//! `#handleSyncMessage`, `#handleReceiptMessage`, `#handleTypingMessage`, `#handleEditMessage`,
//! `processDataMessage.preload.ts`).
//!
//! Mapped here: 1:1 `DataMessage`s (text with formatting and mention ranges, quotes, timers,
//! view-once; reactions; deletes; `END_SESSION`; `EXPIRATION_TIMER_UPDATE` with version rules;
//! profile keys), `EditMessage`, `ReceiptMessage`, server delivery receipts, `TypingMessage`
//! (ephemeral, never while catching up, only with the typing setting on) and the sync messages
//! `sent` (1:1 transcripts, flags included), `read`, `configuration` and
//! `messageRequestResponse`; S9 adds `contacts` (queued as a `contact_sync` job), `blocked`,
//! `keys` and `fetchLatest{STORAGE_MANIFEST}` (a `storage_sync` job), profile fetches for new
//! profile keys, and drops everything a blocked sender's envelope held ([`dispatch_content`]).
//! Backend
//! effects of S8: owed delivery receipts for sealed messages in accepted chats, Message Send
//! Log rows settled by delivery receipts (and the PNI signature they acknowledge), profile
//! sharing from our phone's transcripts and request responses, the synced settings, and the
//! features of every 1:1 chat the events touch (`crate::features`); S12 maps attachments,
//! stickers and long texts (`super::media`).
//!
//! S13 (`SD/ts/util/handleEditMessage.preload.ts`, `SD/ts/messageModifiers/Deletes.preload.ts`,
//! `DeletesForMe.preload.ts`, `ViewSyncs.preload.ts`, `ViewOnceOpenSyncs.preload.ts`,
//! `SD/ts/messages/handleDataMessage.preload.ts`): quotes with the quoted attachment; edits of
//! messages with attachments (the new caption; the core keeps the files); the timer every 1:1
//! message states, with Desktop's version rules ([`crate::timer`]); a group admin's delete of
//! another member's message; `SyncMessage.deleteForMe` (messages, whole chats and single
//! attachments), `viewed` and `viewOnceOpen`; a long text the lane could not download marked
//! for the core to fetch.
//!
//! Nothing a user would see is dropped silently: content Kourier does not show becomes
//! `Body::Unsupported` with a `kind_hint` of its own (`view-once`, `contact`, `payment`,
//! `gift-badge`, `poll`, `poll-ended`, `story-reply`, `pin`, `sticker` for one
//! without an image, `newer-protocol`) and its raw `DataMessage` kept for a later version
//! (never for view-once); a reaction to a story is a `story-reply` too, as Desktop shows it as
//! a message of the chat. Modifiers of such placeholders (poll votes, unpins) and content that
//! is no chat message (stories) are dropped with a log-safe reason.
//!
//! Calls are shown and never taken ([`super::calls`]): a `CallMessage`, a group's
//! `DataMessage.groupCallUpdate` and the phone's `SyncMessage.callEvent` become signals of
//! one call line each. Nothing is sent for any of them: no Busy, no Hangup, no ring response,
//! no receipt (a group-call update is owed no delivery receipt either, as on Desktop, which
//! handles it before the path that queues one) and no sync.
//!
//! Validation (R:signal-desktop-receive §9 items 18, 21, 25) that fails is a *content failure*:
//! it counts towards the poison limit of §5.6 (stage 2 parks and retries the row, then drops it
//! with a placeholder).

use std::time::Duration;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::rusqlite::Connection;
use kourier_model::{
    AccountSettingsPatch, BackendBlob, Body, ContactPatch, ConvKey, ConversationPatch,
    DisappearKind, Expiry, MsgTarget, Origin, Placeholder, QuoteSnapshot, RangeKind, ReceiptKind,
    RecipientState, ReplyTo, SystemEvent, TargetAuthor, Text, TextRange, UserId, Utf16Map,
};
use libsignal_core::{Aci, ServiceId};
use prost::Message as _;

use super::KIND_DELIVERY_RECEIPT;
use super::calls;
use super::group_dispatch;
use super::media;
use super::stage2::Row;
use crate::db::{self, Account};
use crate::error::Result;
use crate::jobs::{self, JobKind};
use crate::proto::{self, content::Content as ContentKind};
use crate::send::outgoing;
use crate::service_id::{aci_from_wire, aci_user_id, service_id_from_wire, service_id_user_id};
use crate::store::archive_all_sessions;
use crate::timer::{self, Source, Stated};
use crate::{commands, contact_sync, features, groups, msl, peers, profile, storage};

/// `DataMessage.Flags.END_SESSION`.
pub(super) const END_SESSION: u32 = 1;
/// `DataMessage.Flags.EXPIRATION_TIMER_UPDATE`.
pub(super) const EXPIRATION_TIMER_UPDATE: u32 = 2;
/// `DataMessage.Flags.PROFILE_KEY_UPDATE`.
pub(super) const PROFILE_KEY_UPDATE: u32 = 4;
/// The only flags `processDataMessage` accepts.
const KNOWN_FLAGS: u32 = END_SESSION | EXPIRATION_TIMER_UPDATE | PROFILE_KEY_UPDATE;
/// `DataMessage.ProtocolVersion.CURRENT`: newer required versions are shown as unsupported.
const CURRENT_PROTOCOL_VERSION: u32 = 8;
/// How long a typing indicator lasts without a refresh (Desktop's 15 s).
pub(crate) const TYPING_TTL: Duration = Duration::from_secs(15);
/// `BackendBlob.v` of a raw `DataMessage` kept with `Body::Unsupported`.
const RAW_DATA_MESSAGE: u16 = 1;

/// What stage 2 needs to map a row.
pub(crate) struct Ctx<'a> {
    /// Our account.
    pub(crate) account: &'a Account,
    /// The row arrived before the catch-up boundary (or it is not known yet, §4.5).
    pub(crate) catch_up: bool,
    /// Wall clock (ms) for side-effect rows.
    pub(crate) now: i64,
    /// The whole text of the row's message when its long-text attachment was downloaded in the
    /// lane's network step ([`super::media::long_texts`]).
    pub(crate) long_text: Option<&'a str>,
}

/// The result of mapping one row.
#[derive(Debug, PartialEq)]
pub(crate) enum Outcome {
    /// Handled: `events` go into `core_outbox` with the row's deletion, `ephemeral` ones to the
    /// core after the commit; `note` says what was dropped, if anything (log-safe).
    Handled {
        /// Durable core events.
        events: Vec<BackendEvent>,
        /// Ephemeral core events (typing).
        ephemeral: Vec<BackendEvent>,
        /// Why nothing (or not everything) was mapped.
        note: Option<&'static str>,
    },
    /// A content failure (decode or validation): counts towards the poison limit. `visible`
    /// rows get a placeholder when they are finally dropped.
    Failed {
        /// Log-safe reason.
        reason: &'static str,
        /// Would have been a timeline message.
        visible: bool,
    },
}

impl Outcome {
    pub(super) fn events(events: Vec<BackendEvent>) -> Self {
        Self::Handled {
            events,
            ephemeral: Vec::new(),
            note: None,
        }
    }

    pub(super) fn dropped(note: &'static str) -> Self {
        Self::Handled {
            events: Vec::new(),
            ephemeral: Vec::new(),
            note: Some(note),
        }
    }

    pub(super) fn failed(reason: &'static str, visible: bool) -> Self {
        Self::Failed { reason, visible }
    }
}

/// Maps one inbox row. Side effects (profile keys, timer versions, archived sessions, owed
/// delivery receipts, the Message Send Log) are written on `conn`, inside the caller's zone; the
/// features of every 1:1 chat the events touch go in front of them when the core does not have
/// them yet. `Err` only for database failures.
pub(crate) fn dispatch(conn: &Connection, ctx: &Ctx<'_>, row: &Row) -> Result<Outcome> {
    Ok(match dispatch_content(conn, ctx, row)? {
        Outcome::Handled {
            events,
            ephemeral,
            note,
        } => Outcome::Handled {
            events: features::with_features(conn, ctx.account, events, ctx.now)?,
            ephemeral,
            note,
        },
        failed @ Outcome::Failed { .. } => failed,
    })
}

fn dispatch_content(conn: &Connection, ctx: &Ctx<'_>, row: &Row) -> Result<Outcome> {
    let sender = ServiceId::parse_from_service_id_string(&row.sender);
    if row.kind == KIND_DELIVERY_RECEIPT {
        return delivery_receipt(conn, ctx, row, sender);
    }
    // Whatever a blocked sender sent is dropped once stage 1 has decrypted it (sessions and
    // sender keys still advance): messages, edits, reactions, receipts, typing, their retry
    // requests, and what of theirs did not decrypt, which gets no retry request and no
    // placeholder (`MessageReceiver.preload.ts` `#decryptEnvelope`: "Dropping message from
    // blocked sender", "Error from blocked sender; no further processing"). The server's own
    // delivery receipt above is not something the sender wrote; Desktop applies it too. The
    // lane's download step fetches no long text for them either (`media::long_texts`).
    if peers::load(conn, &row.sender)?.blocked {
        return Ok(Outcome::dropped("content from a blocked contact"));
    }
    if let Some(dem) = row.decrypt_error.as_deref() {
        return Ok(Outcome::events(crate::retry::on_decryption_failure(
            conn,
            ctx.account,
            row,
            dem,
            ctx.catch_up,
            ctx.now,
        )?));
    }
    let Some(bytes) = row.content.as_deref() else {
        return Ok(Outcome::failed("content row without content", true));
    };
    let Ok(content) = proto::Content::decode(bytes) else {
        return Ok(Outcome::failed("undecodable content", true));
    };
    let has_skdm = content.sender_key_distribution_message.is_some();
    Ok(match content.content {
        Some(ContentKind::DataMessage(message)) => match sender {
            Some(ServiceId::Aci(aci)) => data_message(conn, ctx, row, aci, message)?,
            _ => Outcome::dropped("data message from a non-ACI sender"),
        },
        Some(ContentKind::EditMessage(edit)) => match sender {
            Some(ServiceId::Aci(aci))
                if edit
                    .data_message
                    .as_ref()
                    .is_some_and(|m| m.group_v2.is_some()) =>
            {
                match aci_user_id(aci) {
                    Ok(user) if aci != ctx.account.aci => {
                        group_dispatch::group_edit(conn, ctx, row, aci, user, &edit)?
                    }
                    _ => Outcome::dropped("own group edit outside a sync transcript"),
                }
            }
            Some(ServiceId::Aci(aci)) => edit_message(ctx, row, aci, &edit),
            _ => Outcome::dropped("edit from a non-ACI sender"),
        },
        Some(ContentKind::SyncMessage(sync)) => sync_message(conn, ctx, row, sender, sync)?,
        Some(ContentKind::ReceiptMessage(receipt)) => {
            receipt_message(conn, ctx, row, sender, &receipt)?
        }
        Some(ContentKind::TypingMessage(typing)) => {
            typing_message(conn, ctx, row, sender, &typing)?
        }
        Some(ContentKind::DecryptionErrorMessage(bytes)) => Outcome::dropped(
            crate::retry::on_retry_request(conn, ctx.account, row, &bytes, ctx.now)?,
        ),
        Some(ContentKind::NullMessage(_)) => Outcome::dropped("null message"),
        Some(ContentKind::CallMessage(message)) => {
            calls::call_message(conn, ctx, row, sender, &message)?
        }
        Some(ContentKind::StoryMessage(_)) => Outcome::dropped("story (not in M1)"),
        None if has_skdm => Outcome::dropped("sender key distribution (processed in stage 1)"),
        None => Outcome::dropped("content without a message"),
    })
}

/// A `SERVER_DELIVERY_RECEIPT` envelope: our message with the envelope's timestamp reached a
/// device of the sender (the receipt's source may be a PNI).
fn delivery_receipt(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    sender: Option<ServiceId>,
) -> Result<Outcome> {
    let Some((service_id, by)) =
        sender.and_then(|s| service_id_user_id(s).ok().map(|user| (s, user)))
    else {
        return Ok(Outcome::dropped("delivery receipt without a valid source"));
    };
    delivered(conn, ctx, service_id, row.sender_device, &[row.timestamp])?;
    Ok(Outcome::events(vec![BackendEvent::Receipts {
        targets: vec![own_target(None, row.timestamp)],
        by,
        kind: ReceiptKind::Delivered,
        at: row.server_timestamp.unwrap_or(row.received_at),
    }]))
}

/// A delivery receipt's backend effects (`MessageReceipts.preload.ts` `deleteSentProtoBatcher`):
/// the device no longer needs the logged payloads; a receipt for a message that carried our PNI
/// signature means the contact merged our number, so the signature stops.
fn delivered(
    conn: &Connection,
    ctx: &Ctx<'_>,
    sender: ServiceId,
    device: u32,
    timestamps: &[u64],
) -> Result<()> {
    let sid = sender.service_id_string();
    if msl::delivered(conn, &sid, device, timestamps)? {
        peers::clear_share_my_number(conn, &sid, ctx.now)?;
    }
    Ok(())
}

/// Who wrote a `DataMessage`: another user, or our account on another device (sync transcript).
pub(super) enum Who {
    Remote { user: UserId, device: u32 },
    Own { device: u32 },
}

impl Who {
    /// `by` of modifiers: `None` = own account.
    pub(super) fn by(&self) -> Option<UserId> {
        match self {
            Self::Remote { user, .. } => Some(user.clone()),
            Self::Own { .. } => None,
        }
    }

    /// The author of the writer's own messages as a modifier target.
    fn as_target_author(&self) -> TargetAuthor {
        match self {
            Self::Remote { user, .. } => TargetAuthor::User(user.clone()),
            Self::Own { .. } => TargetAuthor::Own,
        }
    }

    pub(super) fn origin(&self) -> Origin {
        match self {
            Self::Remote { user, device } => Origin::Remote {
                author: user.clone(),
                device: Some(*device),
            },
            Self::Own { device } => Origin::OwnOtherDevice {
                device: Some(*device),
            },
        }
    }
}

/// `processDataMessage` validation (item 18). `timestamp` is the envelope's (or the sync
/// transcript's) timestamp.
///
/// The number of attachments is not checked: Desktop and Android cut a message at 32 for their
/// own display (`ATTACHMENT_MAX`, `toPointersWithinLimit`), which is no reason to hide the
/// rest from the user here. The server bounds a message at 256 KiB, and with it the list.
pub(super) fn validate(
    message: &proto::DataMessage,
    timestamp: u64,
) -> std::result::Result<(), &'static str> {
    if message.timestamp != Some(timestamp) {
        return Err("data message timestamp differs from the envelope");
    }
    if message.flags.unwrap_or(0) & !KNOWN_FLAGS != 0 {
        return Err("data message with unknown flags");
    }
    if message
        .group_v2
        .as_ref()
        .and_then(|g| g.master_key.as_deref())
        .is_some_and(|key| key.len() != 32)
    {
        return Err("group master key is not 32 bytes");
    }
    Ok(())
}

/// A `DataMessage` from another user.
fn data_message(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    message: proto::DataMessage,
) -> Result<Outcome> {
    if aci == ctx.account.aci {
        return Ok(Outcome::dropped(
            "own data message outside a sync transcript",
        ));
    }
    if let Err(reason) = validate(&message, row.timestamp) {
        return Ok(Outcome::failed(reason, true));
    }
    let Ok(user) = aci_user_id(aci) else {
        return Ok(Outcome::dropped("sender id"));
    };
    if let Some(key) = message.profile_key.as_deref().filter(|k| k.len() == 32) {
        // A new profile key (fetched again even if a fetch is running with the old one), or
        // the first message of a contact whose profile we never fetched (§6.10 cadence).
        if peers::store_profile_key(conn, aci, key, ctx.now)? {
            profile::request(conn, &row.sender, ctx.now)?;
        } else if !profile::fetched(conn, &row.sender)? {
            profile::queue(conn, &row.sender, ctx.now)?;
        }
    }
    peers::mark_registered(conn, &row.sender, ctx.now)?;
    let outcome = data_message_events(conn, ctx, row, aci, user, &message)?;
    let accepted = || -> Result<bool> {
        if message.group_v2.is_some() {
            group_dispatch::delivery_receipts_owed(conn, &message)
        } else {
            features::is_accepted(conn, ctx.account, ServiceId::from(aci))
        }
    };
    if row.sealed
        && let Outcome::Handled { events, .. } = &outcome
        && events
            .iter()
            .any(|e| matches!(e, BackendEvent::Message(m) if m.body.call_line().is_none()))
        && accepted()?
    {
        // Delivery receipts only for sealed messages in accepted chats (Desktop's
        // `handleDataMessage.preload.ts:371-397`); unsealed ones get the server's receipt.
        // Never for a call line (a group-call update): nothing is sent for a call, and
        // Desktop consumes that update before the path that queues a receipt
        // (`background.preload.ts` `handleGroupCallUpdateMessage`).
        commands::queue_delivery_receipt(conn, &row.sender, row.timestamp, ctx.now)?;
    }
    Ok(outcome)
}

/// The events of a validated `DataMessage` from another user.
fn data_message_events(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    user: UserId,
    message: &proto::DataMessage,
) -> Result<Outcome> {
    let flags = message.flags.unwrap_or(0);
    if flags & PROFILE_KEY_UPDATE != 0 {
        // `ProfileKeyUpdateEvent`: the key was stored and the profile is fetched again
        // (`data_message`); nothing to show.
        return Ok(Outcome::dropped(
            "profile key update (key stored, profile refetched)",
        ));
    }
    if message.group_v2.is_some() {
        return group_dispatch::group_message(conn, ctx, row, aci, user, message);
    }
    let conv = ConvKey::Direct(user.clone());
    let who = Who::Remote {
        user: user.clone(),
        device: row.sender_device,
    };
    if flags & END_SESSION != 0 {
        // Desktop's `handleEndSession`: the sender reset the session; every session with them
        // is archived and the timeline shows a notice.
        archive_all_sessions(conn, &row.sender, ctx.now)?;
        return Ok(Outcome::events(vec![system_message(
            ctx,
            row,
            &who,
            conv,
            SystemEvent::SessionRefreshed { user },
        )]));
    }
    if flags & EXPIRATION_TIMER_UPDATE != 0 {
        return timer_update(
            conn,
            ctx,
            row,
            &row.sender,
            &who,
            conv,
            message,
            row.timestamp,
        );
    }
    let at = row
        .server_timestamp
        .unwrap_or_else(|| i64::try_from(row.timestamp).unwrap_or(i64::MAX));
    let outcome = content_events(ctx, row, &who, conv.clone(), message, row.timestamp, at);
    with_stated_timer(
        conn,
        ctx,
        row,
        &row.sender,
        &who,
        &conv,
        message,
        row.timestamp,
        outcome,
    )
}

/// Who and what a timer notice of a message from `who` sent at `timestamp` is filed under.
fn timer_source(ctx: &Ctx<'_>, row: &Row, who: &Who, local: String, sent_at: i64) -> Source {
    Source {
        origin: who.origin(),
        by: who.by(),
        local,
        sent_at,
        received_at: row.received_at,
        server_at: row.server_timestamp,
        arrival: row.arrival,
        catch_up: ctx.catch_up,
    }
}

/// `EXPIRATION_TIMER_UPDATE` in the 1:1 chat with `peer` (a service id string): the timer the
/// update states, under the version rules of [`timer::incoming`] (an update without a version
/// applies, one with an older version does not). The notice is the update itself: it has the
/// update's id.
#[allow(
    clippy::too_many_arguments,
    reason = "the row, the writer, the chat and the message are distinct inputs"
)]
fn timer_update(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    peer: &str,
    who: &Who,
    conv: ConvKey,
    message: &proto::DataMessage,
    timestamp: u64,
) -> Result<Outcome> {
    let source = timer_source(
        ctx,
        row,
        who,
        timestamp.to_string(),
        i64::try_from(timestamp).unwrap_or(i64::MAX),
    );
    let events = timer::incoming(
        conn,
        peer,
        &conv,
        Stated {
            timer: message.expire_timer.unwrap_or(0),
            version: message.expire_timer_version.unwrap_or(0),
            update: true,
        },
        source,
        ctx.now,
    )?;
    Ok(if events.is_empty() {
        Outcome::dropped("timer update without a change (an older version, or the same timer)")
    } else {
        Outcome::events(events)
    })
}

/// Desktop's `handleDataMessage` applies the timer every 1:1 message states to its chat, not
/// only the one of a timer update: a timeline message whose timer differs from the chat's
/// changes it (Desktop's version rules; a message without a version changes nothing), and the
/// notice stands above the message.
#[allow(
    clippy::too_many_arguments,
    reason = "the row, the writer, the chat and the message are distinct inputs"
)]
fn with_stated_timer(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    peer: &str,
    who: &Who,
    conv: &ConvKey,
    message: &proto::DataMessage,
    timestamp: u64,
    outcome: Outcome,
) -> Result<Outcome> {
    let Outcome::Handled {
        events,
        ephemeral,
        note,
    } = outcome
    else {
        return Ok(outcome);
    };
    if !events.iter().any(|e| matches!(e, BackendEvent::Message(_))) {
        return Ok(Outcome::Handled {
            events,
            ephemeral,
            note,
        });
    }
    let sent_at = i64::try_from(timestamp)
        .unwrap_or(i64::MAX)
        .saturating_sub(1);
    let source = timer_source(ctx, row, who, format!("timer:{timestamp}"), sent_at);
    let mut all = timer::incoming(
        conn,
        peer,
        conv,
        Stated {
            timer: message.expire_timer.unwrap_or(0),
            version: message.expire_timer_version.unwrap_or(0),
            update: false,
        },
        source,
        ctx.now,
    )?;
    all.extend(events);
    Ok(Outcome::Handled {
        events: all,
        ephemeral,
        note,
    })
}

/// A timeline notice that a `DataMessage` itself stands for (it has the message's id: author
/// and sent timestamp).
fn system_message(
    ctx: &Ctx<'_>,
    row: &Row,
    who: &Who,
    conv: ConvKey,
    event: SystemEvent,
) -> BackendEvent {
    let mut message = IncomingMessage::new(
        conv,
        row.timestamp.to_string(),
        who.origin(),
        Body::System(event),
        i64::try_from(row.timestamp).unwrap_or(i64::MAX),
        row.received_at,
    );
    message.server_at = row.server_timestamp;
    message.arrival = Some(row.arrival);
    message.catch_up = ctx.catch_up;
    BackendEvent::Message(message)
}

/// The events of a validated 1:1 `DataMessage` from `who` in `conv` sent at `timestamp`:
/// a modifier (reaction, delete), or a timeline message. `at` dates modifiers.
pub(super) fn content_events(
    ctx: &Ctx<'_>,
    row: &Row,
    who: &Who,
    conv: ConvKey,
    message: &proto::DataMessage,
    timestamp: u64,
    at: i64,
) -> Outcome {
    // A reaction to a story names the story, not a message of the chat: Desktop shows it in
    // the chat as a message of its own ("Reacted to your story",
    // `SD/ts/messageModifiers/Reactions.preload.ts`), so here it is a story-reply placeholder
    // (`message_body`) instead of a reaction waiting for a target that never comes.
    if let Some(reaction) = &message.reaction
        && message.story_context.is_none()
    {
        return reaction_event(ctx, who, conv, reaction, at);
    }
    if let Some(delete) = &message.delete {
        let Some(target) = delete.target_sent_timestamp else {
            return Outcome::dropped("delete without a target");
        };
        return Outcome::events(vec![BackendEvent::Deleted {
            target: MsgTarget {
                conv: Some(conv),
                author: who.as_target_author(),
                local: target.to_string().into(),
                alternates: Vec::new(),
            },
            by: who.by(),
            for_everyone: true,
            at,
        }]);
    }
    if let Some(delete) = &message.admin_delete {
        // A group admin deletes another member's message: the core checks the sender's role
        // (`features.admin_delete`) and the window, as it checks authorship above.
        if !matches!(conv, ConvKey::Group(_)) {
            return Outcome::dropped("admin delete outside a group");
        }
        let (Some(target), Some(author)) = (
            delete.target_sent_timestamp,
            target_author(ctx, delete.target_author_aci_binary.as_deref(), None),
        ) else {
            return Outcome::dropped("admin delete without a target");
        };
        return Outcome::events(vec![BackendEvent::Deleted {
            target: MsgTarget {
                conv: Some(conv),
                author,
                local: target.to_string().into(),
                alternates: Vec::new(),
            },
            by: who.by(),
            for_everyone: true,
            at,
        }]);
    }
    if message.poll_vote.is_some() {
        return Outcome::dropped("poll vote (polls are placeholders)");
    }
    if message.unpin_message.is_some() {
        return Outcome::dropped("unpin (pins are placeholders)");
    }
    let Some(body) = message_body(ctx, message) else {
        return Outcome::dropped("data message without content");
    };
    let mentions_me = match &body {
        Body::Text(text) => mentions(ctx, text),
        Body::Media {
            caption: Some(text),
            ..
        } => mentions(ctx, text),
        _ => false,
    };
    let mut incoming = IncomingMessage::new(
        conv.clone(),
        timestamp.to_string(),
        who.origin(),
        body,
        i64::try_from(timestamp).unwrap_or(i64::MAX),
        row.received_at,
    );
    let view_once = message.is_view_once.unwrap_or(false);
    // A view-once message is never kept raw: its pointer and key would outlive the one view.
    if matches!(incoming.body, Body::Unsupported { .. }) && !view_once {
        incoming.raw = Some(BackendBlob::new(RAW_DATA_MESSAGE, message.encode_to_vec()));
    }
    incoming.server_at = row.server_timestamp;
    incoming.arrival = Some(row.arrival);
    incoming.view_once = view_once;
    incoming.mentions_me = mentions_me;
    incoming.reply_to = message
        .quote
        .as_ref()
        .and_then(|quote| reply_to(ctx, &conv, quote));
    // The sender's disappearing timer: a received message starts it once read; our own message
    // from another device started it when sent (`expirationStartTimestamp`, set by the caller).
    incoming.expiry = message
        .expire_timer
        .filter(|secs| *secs > 0)
        .map(|secs| Expiry {
            kind: match who {
                Who::Remote { .. } => DisappearKind::AfterRead,
                Who::Own { .. } => DisappearKind::AfterSend,
            },
            timer: Duration::from_secs(u64::from(secs)),
            started_at: None,
        });
    incoming.catch_up = ctx.catch_up;
    Outcome::events(vec![BackendEvent::Message(incoming)])
}

/// `DataMessage.reaction` → `Reaction` on the target (ours or the sender's). Its emoji must be
/// one emoji, as on Desktop ([`crate::emoji`]): anything else is dropped, so no text shows as
/// a reaction.
fn reaction_event(
    ctx: &Ctx<'_>,
    who: &Who,
    conv: ConvKey,
    reaction: &proto::data_message::Reaction,
    at: i64,
) -> Outcome {
    let (Some(emoji), Some(target)) = (
        reaction.emoji.as_ref().filter(|e| !e.is_empty()),
        reaction.target_sent_timestamp,
    ) else {
        return Outcome::dropped("reaction without emoji or target");
    };
    if !crate::emoji::is_one(emoji) {
        return Outcome::dropped("reaction with something that is not one emoji");
    }
    let Some(author) = target_author(
        ctx,
        reaction.target_author_aci_binary.as_deref(),
        reaction.target_author_aci.as_deref(),
    ) else {
        return Outcome::dropped("reaction without a valid target author");
    };
    Outcome::events(vec![BackendEvent::Reaction {
        target: MsgTarget {
            conv: Some(conv),
            author,
            local: target.to_string().into(),
            alternates: Vec::new(),
        },
        by: who.by(),
        emoji: emoji.clone(),
        remove: reaction.remove.unwrap_or(false),
        at,
    }])
}

/// The author of a referenced message from its wire ACI: our own ACI is `Own`.
fn target_author(
    ctx: &Ctx<'_>,
    binary: Option<&[u8]>,
    string: Option<&str>,
) -> Option<TargetAuthor> {
    let aci = aci_from_wire(binary, string).ok().flatten()?;
    if aci == ctx.account.aci {
        return Some(TargetAuthor::Own);
    }
    aci_user_id(aci).ok().map(TargetAuthor::User)
}

/// The body of a timeline message: text (with ranges; the whole text when its long-text
/// attachment was downloaded, else the trimmed body with that attachment as a file),
/// attachments with the text as caption, a sticker, or `Unsupported` for content Kourier does
/// not show yet. `None` for a message without content.
///
/// `groupCallUpdate` is not read here: in a group it is a call line
/// (`calls::group_call_update`, before this is reached), and outside a group it means nothing
/// and the rest of the message is taken as it is (Desktop: "a group call update for a
/// conversation that is not a GV2 group. Ignoring that property").
///
/// View-once media is `Unsupported` ("view-once") and none of its attachments are mapped:
/// until Kourier can show such media once and then forget it, it must not download it by the
/// core's policy and keep it in the timeline like any other picture (Desktop shows it once and
/// erases it, `SD/ts/services/MessageUpdater.preload.ts` `markViewOnceMessageViewed`, or after
/// the message queue time, `tapToViewMessagesDeletionService.preload.ts`).
fn message_body(ctx: &Ctx<'_>, message: &proto::DataMessage) -> Option<Body> {
    let unsupported = |kind: &str| Body::Unsupported {
        kind_hint: kind.to_owned(),
        summary: None,
    };
    if message
        .required_protocol_version
        .is_some_and(|v| v > CURRENT_PROTOCOL_VERSION)
    {
        return Some(unsupported("newer-protocol"));
    }
    if message.is_view_once.unwrap_or(false) {
        return Some(unsupported("view-once"));
    }
    let kind = if !message.contact.is_empty() {
        Some("contact")
    } else if message.payment.is_some() {
        Some("payment")
    } else if message.gift_badge.is_some() {
        Some("gift-badge")
    } else if message.poll_create.is_some() {
        Some("poll")
    } else if message.poll_terminate.is_some() {
        Some("poll-ended")
    } else if message.story_context.is_some() {
        Some("story-reply")
    } else if message.pin_message.is_some() {
        Some("pin")
    } else {
        None
    };
    if let Some(kind) = kind {
        // Shown by a later version from the raw DataMessage kept with it.
        return Some(unsupported(kind));
    }
    if let Some(sticker) = &message.sticker {
        return Some(match media::sticker(sticker) {
            Some(image) => Body::Sticker(Box::new(image)),
            None => unsupported("sticker"),
        });
    }
    let text = ctx
        .long_text
        .filter(|t| !t.is_empty())
        .or_else(|| message.body.as_deref().filter(|b| !b.is_empty()))
        .map(|body| text(body, &message.body_ranges));
    // Once the lane downloaded the long text, the body is the whole text and its attachment
    // is not shown. When it could not (the CDN failed, the step ran out of time), the
    // attachment stays with the message, marked as the rest of its text
    // (`Attachment::long_text`): the core downloads it whatever the download settings say and
    // joins it into the body, so the trimmed body is never passed off as the whole text.
    let whole_text = ctx.long_text.is_some_and(|t| !t.is_empty());
    let attachments: Vec<_> = message
        .attachments
        .iter()
        .filter(|p| !(whole_text && media::is_long_text(p)))
        .map(|p| {
            let mut attachment = media::attachment(p, None);
            attachment.long_text = media::is_long_text(p);
            attachment
        })
        .collect();
    if attachments.is_empty() {
        return text.map(Body::Text);
    }
    Some(Body::Media {
        attachments,
        caption: text,
    })
}

/// A Signal body with its `BodyRange`s (UTF-16 offsets) as a Kourier [`Text`] (UTF-8 offsets);
/// ranges that do not map or fit are dropped (the core drops invalid ones again on ingest).
fn text(body: &str, ranges: &[proto::BodyRange]) -> Text {
    use proto::body_range::{AssociatedValue, Style};
    ranged_text(
        body,
        ranges.iter().filter_map(|range| {
            let kind = match &range.associated_value {
                Some(AssociatedValue::Style(style)) => match Style::try_from(*style) {
                    Ok(Style::Bold) => RangeKind::Bold,
                    Ok(Style::Italic) => RangeKind::Italic,
                    Ok(Style::Spoiler) => RangeKind::Spoiler,
                    Ok(Style::Strikethrough) => RangeKind::Strike,
                    Ok(Style::Monospace) => RangeKind::Mono,
                    Ok(Style::None) | Err(_) => return None,
                },
                Some(AssociatedValue::MentionAci(aci)) => mention(None, Some(aci))?,
                Some(AssociatedValue::MentionAciBinary(aci)) => mention(Some(aci), None)?,
                None => return None,
            };
            Some((range.start?, range.length?, kind))
        }),
    )
}

/// `body` with `ranges` given as (start, length) in UTF-16 code units, the way Signal counts
/// them on the wire and in a transfer archive, as a Kourier [`Text`] with UTF-8 byte offsets.
/// A range that does not map (inside a surrogate pair, past the end) or does not fit the text
/// (a mention that does not cover its placeholder) is dropped. The live mapper and the history
/// import's (`transfer::map`) both go through this.
pub(crate) fn ranged_text(
    body: impl Into<String>,
    ranges: impl IntoIterator<Item = (u32, u32, RangeKind)>,
) -> Text {
    let body = body.into();
    let map = Utf16Map::new(&body);
    let mut text = Text::plain(body);
    for (start, length, kind) in ranges {
        let Some((start, len)) = map.range_to_utf8(start, length) else {
            continue;
        };
        text.ranges.push(TextRange { start, len, kind });
    }
    text.sanitize();
    text
}

/// A mention range of a user.
pub(crate) fn mention(binary: Option<&[u8]>, string: Option<&str>) -> Option<RangeKind> {
    let aci = aci_from_wire(binary, string).ok().flatten()?;
    aci_user_id(aci).ok().map(RangeKind::Mention)
}

/// Whether `text` mentions our ACI.
fn mentions(ctx: &Ctx<'_>, text: &Text) -> bool {
    let Ok(me) = aci_user_id(ctx.account.aci) else {
        return false;
    };
    text.ranges
        .iter()
        .any(|range| matches!(&range.kind, RangeKind::Mention(user) if *user == me))
}

/// `DataMessage.quote` → the quoted message and the sender's snapshot of it: text with ranges,
/// and kind, name and type of the quoted attachment.
fn reply_to(ctx: &Ctx<'_>, conv: &ConvKey, quote: &proto::data_message::Quote) -> Option<ReplyTo> {
    let id = quote.id?;
    let author = target_author(
        ctx,
        quote.author_aci_binary.as_deref(),
        quote.author_aci.as_deref(),
    )?;
    let quoted_user = match &author {
        TargetAuthor::User(user) => Some(user.clone()),
        TargetAuthor::Own | TargetAuthor::Unknown => None,
    };
    let snapshot_text = quote
        .text
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| text(t, &quote.body_ranges));
    // The first quoted attachment as the sender named it (`Quote.QuotedAttachment`): its type
    // says what kind of file the quote shows; the thumbnail is not fetched.
    let file = quote.attachments.first();
    let mime = file.and_then(|f| media::kept_mime(f.content_type.as_deref()));
    Some(ReplyTo {
        target: MsgTarget {
            conv: Some(conv.clone()),
            author,
            local: id.to_string().into(),
            alternates: Vec::new(),
        },
        author: quoted_user,
        snapshot: Some(QuoteSnapshot {
            text: snapshot_text,
            attachment: file.map(|_| media::kind_of(mime.as_deref().unwrap_or(""), 0)),
            file_name: file
                .and_then(|f| f.file_name.clone())
                .filter(|n| !n.is_empty() && n.len() <= 255),
            mime,
            thumb: None,
        }),
    })
}

/// `EditMessage` from another user (`#handleEditMessage`): the new revision's text; the core
/// checks the author and the 48 h window (§3.8).
fn edit_message(ctx: &Ctx<'_>, row: &Row, aci: Aci, edit: &proto::EditMessage) -> Outcome {
    if aci == ctx.account.aci {
        return Outcome::dropped("own edit outside a sync transcript");
    }
    let Ok(user) = aci_user_id(aci) else {
        return Outcome::dropped("sender id");
    };
    let who = Who::Remote {
        user: user.clone(),
        device: row.sender_device,
    };
    edit_events(
        &who,
        ConvKey::Direct(user),
        edit,
        row.timestamp,
        edit_time(row, row.timestamp),
        ctx.long_text,
    )
}

/// When an edit from another user happened, for the receive window: the server's timestamp of
/// its envelope, as Desktop (`isTooOldToEditMessage(serverTimestamp, …)`); the sender's own
/// timestamp could be dated back into the window.
pub(super) fn edit_time(row: &Row, timestamp: u64) -> i64 {
    row.server_timestamp
        .unwrap_or_else(|| i64::try_from(timestamp).unwrap_or(i64::MAX))
}

/// The `Edited` event of an edit sent at `timestamp` by `who` in `conv`: the new revision,
/// dated `at`. `long_text` is the whole text when the edit carries a long-text attachment and
/// the lane downloaded it ([`Ctx::long_text`]); while it could not, the edit fails and is tried
/// again, so a revision is never cut to its first 2 KiB (a long-text pointer nothing can be
/// fetched from gives the trimmed body at once). An edit that carries other attachments is the
/// new caption of a message with files: its body is `Body::Media` with the caption (none when
/// the edit has no text), and the core keeps the message's own files whatever the edit lists
/// (`BackendEvent::Edited`).
pub(super) fn edit_events(
    who: &Who,
    conv: ConvKey,
    edit: &proto::EditMessage,
    timestamp: u64,
    at: i64,
    long_text: Option<&str>,
) -> Outcome {
    let (Some(target), Some(message)) = (edit.target_sent_timestamp, edit.data_message.as_ref())
    else {
        return Outcome::failed("edit without target or message", true);
    };
    if let Err(reason) = validate(message, timestamp) {
        return Outcome::failed(reason, true);
    }
    // No client applies an edit to a view-once message, a contact share or a voice message
    // (`SD/ts/util/handleEditMessage.preload.ts`; Android `EditMessageProcessor.kt`
    // `validTarget`; iOS `EditManagerImpl.swift` `checkForValidEdit`), so one applied here
    // would show on this device alone. An edit states the whole new revision, files included:
    // one that is itself any of the three edits such a message. What the *target* is, only
    // the core knows (it has the row); it checks that side.
    if message.is_view_once.unwrap_or(false) {
        return Outcome::dropped("edit of a view-once message");
    }
    if !message.contact.is_empty() {
        return Outcome::dropped("edit of a contact share");
    }
    if message
        .attachments
        .iter()
        .any(|p| p.flags.unwrap_or(0) & crate::send::media::FLAG_VOICE != 0)
    {
        return Outcome::dropped("edit of a voice message");
    }
    let files: Vec<_> = message
        .attachments
        .iter()
        .filter(|p| !media::is_long_text(p))
        .map(|p| media::attachment(p, None))
        .collect();
    let body = match long_text.filter(|t| !t.is_empty()) {
        Some(whole) => Some(whole),
        None if message
            .attachments
            .iter()
            .any(media::is_fetchable_long_text) =>
        {
            return Outcome::failed("the long text of an edit is not downloaded yet", false);
        }
        None => message.body.as_deref().filter(|b| !b.is_empty()),
    };
    let caption = body.map(|body| text(body, &message.body_ranges));
    let body = if files.is_empty() {
        match caption {
            Some(text) => Body::Text(text),
            None => return Outcome::dropped("edit without text"),
        }
    } else {
        Body::Media {
            attachments: files,
            caption,
        }
    };
    Outcome::events(vec![BackendEvent::Edited {
        target: MsgTarget {
            conv: Some(conv),
            author: who.as_target_author(),
            local: target.to_string().into(),
            alternates: Vec::new(),
        },
        by: who.by(),
        new_local: Some(timestamp.to_string().into()),
        body,
        edited_at: at,
        // A quote can be removed by an edit but not changed: the receivers keep the
        // original's quote exactly when the edit carries one, whatever it says
        // (`SD/ts/util/handleEditMessage.preload.ts`: `editMessageHasQuote ? mainMessage.quote
        // : undefined`; Android `messages/EditMessageProcessor.kt`: `targetQuote != null &&
        // message.quote != null`). The core does the same with this flag.
        keeps_quote: Some(message.quote.is_some()),
    }])
}

/// A `SyncMessage` (`#handleSyncMessage`): only from our own ACI and never from this device
/// (item 21).
fn sync_message(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    sender: Option<ServiceId>,
    sync: proto::SyncMessage,
) -> Result<Outcome> {
    use proto::sync_message::Content as Sync;
    if sender != Some(ServiceId::Aci(ctx.account.aci)) {
        return Ok(Outcome::dropped("sync message from another account"));
    }
    if row.sender_device == u32::from(ctx.account.device_id) {
        return Ok(Outcome::dropped("sync message from this device"));
    }
    if !sync.read.is_empty() {
        return Ok(read_sync(ctx, &sync.read));
    }
    if !sync.viewed.is_empty() {
        let entries: Vec<_> = sync
            .viewed
            .iter()
            .map(|v| {
                (
                    v.sender_aci_binary.as_deref(),
                    v.sender_aci.as_deref(),
                    v.timestamp,
                )
            })
            .collect();
        return Ok(viewed_elsewhere(ctx, row, &entries));
    }
    if !sync.sticker_pack_operation.is_empty() {
        return Ok(Outcome::dropped("sticker pack sync (not in M1)"));
    }
    Ok(match sync.content {
        Some(Sync::Sent(sent)) => sent_transcript(conn, ctx, row, &sent)?,
        Some(Sync::Configuration(configuration)) => {
            // The send path reads them (receipts, typing; §6.7 step 6).
            db::store_configuration(
                conn,
                configuration.read_receipts,
                configuration.typing_indicators,
            )?;
            Outcome::events(vec![BackendEvent::AccountSettings(AccountSettingsPatch {
                read_receipts: configuration.read_receipts,
                typing_indicators: configuration.typing_indicators,
                link_previews: configuration.link_previews,
                sealed_sender_indicators: configuration.unidentified_delivery_indicators,
                ..AccountSettingsPatch::default()
            })])
        }
        Some(Sync::MessageRequestResponse(response)) => {
            message_request_response(conn, ctx, &response)?
        }
        Some(Sync::Contacts(contacts)) => {
            // Downloaded and applied by the `contact_sync` job (no network in a lane step).
            contact_sync::enqueue(conn, &contacts, ctx.now)?;
            Outcome::dropped("contact sync queued")
        }
        Some(Sync::Blocked(blocked)) => blocked_sync(conn, ctx, &blocked)?,
        Some(Sync::Keys(keys)) => {
            if storage::apply_keys(
                conn,
                keys.account_entropy_pool.as_deref(),
                keys.master.as_deref(),
                ctx.now,
            )? {
                tracing::info!("new storage key from our phone: full Storage Service sync");
            }
            Outcome::dropped("keys stored")
        }
        Some(Sync::FetchLatest(fetch)) => {
            use proto::sync_message::fetch_latest::Type;
            match fetch.r#type.and_then(|t| Type::try_from(t).ok()) {
                Some(Type::StorageManifest) => {
                    jobs::request(conn, JobKind::StorageSync, "", ctx.now, None)?;
                    Outcome::dropped("storage sync queued")
                }
                Some(Type::LocalProfile) => Outcome::dropped("own profile fetch (not in M1)"),
                _ => Outcome::dropped("fetch latest of another kind"),
            }
        }
        Some(Sync::DeleteForMe(delete)) => delete_for_me(ctx, row, &delete),
        // The view-once message is a placeholder here (`message_body`); opening it on the
        // phone leaves it viewed (`ViewOnceOpenSyncs.preload.ts` marks it viewed and erases
        // media Kourier never had).
        Some(Sync::ViewOnceOpen(open)) => viewed_elsewhere(
            ctx,
            row,
            &[(
                open.sender_aci_binary.as_deref(),
                open.sender_aci.as_deref(),
                open.timestamp,
            )],
        ),
        // Stage 1 applies a valid one in its own zone (`crate::number_change`); what reaches
        // stage 2 was refused there.
        Some(Sync::PniChangeNumber(_)) => Outcome::dropped("number change (not applied)"),
        Some(Sync::Request(_) | Sync::Verified(_)) => {
            Outcome::dropped("sync request or verified (ignored by linked devices)")
        }
        // The phone's word on a call: answered, declined, unanswered, deleted.
        Some(Sync::CallEvent(event)) => calls::call_event(conn, ctx, row, &event)?,
        // The Calls tab of the phone (clear, mark as read) and call links: Kourier has
        // neither a call log nor call links. A cleared log does not remove the lines of
        // the chats.
        Some(Sync::CallLogEvent(_) | Sync::CallLinkUpdate(_)) => {
            Outcome::dropped("call log or call link sync (not shown)")
        }
        // Only the primary answers a request for lost files; Kourier is never the primary
        // (Desktop and a linked Android drop it too).
        Some(Sync::AttachmentBackfillRequest(_)) => {
            Outcome::dropped("attachment backfill request (only the primary answers)")
        }
        Some(Sync::AttachmentBackfillResponse(response)) => backfill_response(ctx, row, &response),
        Some(Sync::OutgoingPayment(_) | Sync::DeviceNameChange(_) | Sync::UsernameChange(_)) => {
            Outcome::dropped("sync message not in M1")
        }
        None => Outcome::dropped("empty sync message"),
    })
}

/// `SyncMessage.blocked`: the full list of blocked ACIs (and numbers, groups) on our phone
/// (`MessageReceiver.preload.ts` `_handleBlocked`). Contacts the previous list named and this
/// one does not are unblocked (Desktop diffs against its stored list the same way); contacts
/// blocked through the Storage Service stay blocked. Chats are updated only when the core
/// already has them (an empty chat per blocked contact would flood the list); messages from
/// blocked senders are dropped in any case.
fn blocked_sync(
    conn: &Connection,
    ctx: &Ctx<'_>,
    blocked: &proto::sync_message::Blocked,
) -> Result<Outcome> {
    let mut acis: Vec<Aci> = Vec::new();
    for raw in blocked
        .acis_binary
        .iter()
        .map(|b| aci_from_wire(Some(b), None))
        .chain(blocked.acis.iter().map(|s| aci_from_wire(None, Some(s))))
        .chain(
            blocked
                .blocked_acis
                .iter()
                .map(|b| aci_from_wire(b.aci_binary.as_deref(), None)),
        )
    {
        if let Ok(Some(aci)) = raw
            && aci != ctx.account.aci
            && !acis.contains(&aci)
        {
            acis.push(aci);
        }
    }
    let listed: Vec<String> = acis
        .iter()
        .map(|aci| ServiceId::from(*aci).service_id_string())
        .collect();
    let previously: Vec<String> = conn
        .prepare_cached("SELECT service_id FROM blocked_sync")?
        .query_map([], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    conn.execute("DELETE FROM blocked_sync", [])?;
    for sid in &listed {
        conn.prepare_cached("INSERT OR IGNORE INTO blocked_sync(service_id) VALUES (?1)")?
            .execute([sid])?;
    }
    let group_ids: Vec<Vec<u8>> = blocked
        .group_ids
        .iter()
        .cloned()
        .chain(
            blocked
                .blocked_groups
                .iter()
                .filter_map(|g| g.group_id.clone()),
        )
        .collect();
    let mut events = group_dispatch::blocked_groups(conn, ctx, &group_ids)?;
    let changes = listed.iter().map(|sid| (sid.clone(), true)).chain(
        previously
            .into_iter()
            .filter(|sid| !listed.contains(sid))
            .map(|sid| (sid, false)),
    );
    for (sid, block) in changes {
        let Some(service_id) = ServiceId::parse_from_service_id_string(&sid) else {
            continue;
        };
        let Ok(user) = service_id_user_id(service_id) else {
            continue;
        };
        if !peers::set_blocked(conn, &sid, block, ctx.now)? {
            continue;
        }
        let mut contact = ContactPatch::new(user.clone());
        contact.blocked = Some(block);
        events.push(BackendEvent::ContactUpsert(contact));
        if peers::load(conn, &sid)?.features_hash.is_some() {
            let mut patch = ConversationPatch::new(ConvKey::Direct(user));
            patch.blocked = Some(block);
            events.push(BackendEvent::ConversationUpsert(patch));
        }
    }
    Ok(if events.is_empty() {
        Outcome::dropped("blocked list unchanged")
    } else {
        Outcome::events(events)
    })
}

/// A message another device of ours saw: its sender (binary or string ACI) and timestamp.
type Seen<'a> = (Option<&'a [u8]>, Option<&'a str>, Option<u64>);

/// `SyncMessage.viewed` and `.viewOnceOpen`: we viewed these messages (sender, timestamp) on
/// another device (`ViewSyncs.preload.ts`): a `Viewed` receipt of our own account on each,
/// which raises its read state.
fn viewed_elsewhere(ctx: &Ctx<'_>, row: &Row, entries: &[Seen<'_>]) -> Outcome {
    let Ok(me) = aci_user_id(ctx.account.aci) else {
        return Outcome::dropped("own id");
    };
    let targets: Vec<MsgTarget> = entries
        .iter()
        .filter_map(|(binary, string, timestamp)| {
            Some(MsgTarget {
                conv: None,
                author: target_author(ctx, *binary, *string)?,
                local: (*timestamp)?.to_string().into(),
                alternates: Vec::new(),
            })
        })
        .collect();
    if targets.is_empty() {
        return Outcome::dropped("viewed sync without valid entries");
    }
    Outcome::events(vec![BackendEvent::Receipts {
        targets,
        by: me,
        kind: ReceiptKind::Viewed,
        at: i64::try_from(row.timestamp).unwrap_or(i64::MAX),
    }])
}

/// The primary's answer to a request for the files of one message (ours or another device's:
/// the phone sends every answer to all our devices), as `AttachmentRestore`
/// ([`crate::backfill::answer`]).
///
/// - Only from device 1 of our account (the primary, the only device that answers; the sync
///   check above already requires our ACI). Desktop and Android take one from any of our
///   devices; Kourier does not, so a linked device cannot give its files a pointer.
/// - The message is named as the request named it (binary or string service ids; the answer
///   echoes the request), WITH its conversation: the core finds the message only in that
///   conversation, so an answer whose conversation does not match the message changes
///   nothing. A target by phone number alone, or one missing, is dropped with a reason.
fn backfill_response(
    ctx: &Ctx<'_>,
    row: &Row,
    response: &proto::sync_message::AttachmentBackfillResponse,
) -> Outcome {
    if row.sender_device != PRIMARY_DEVICE {
        return Outcome::dropped(
            "attachment backfill answer from a device that is not the primary",
        );
    }
    let Some(conv) = response.target_conversation.as_ref().and_then(thread_conv) else {
        return Outcome::dropped("attachment backfill answer without a conversation it can name");
    };
    let Some(message) = response.target_message.as_ref() else {
        return Outcome::dropped("attachment backfill answer without a message");
    };
    let (Some(author), Some(sent)) = (addressable_author(ctx, message), message.sent_timestamp)
    else {
        return Outcome::dropped("attachment backfill answer without a message it can name");
    };
    let Some(update) = crate::backfill::answer(response) else {
        return Outcome::dropped("empty attachment backfill answer");
    };
    Outcome::events(vec![BackendEvent::AttachmentRestore {
        target: MsgTarget {
            conv: Some(conv),
            author,
            local: sent.to_string().into(),
            alternates: Vec::new(),
        },
        update,
        at: i64::try_from(row.timestamp).unwrap_or(i64::MAX),
        // An answer names no command: the phone answers about the message.
        cmd: None,
    }])
}

/// The device id of an account's primary (the phone that registered it).
const PRIMARY_DEVICE: u32 = 1;

/// The conversation a sync message names (`ConversationIdentifier`): a 1:1 chat by service id
/// (ours is Note to Self) or a group by id. A chat named by phone number alone cannot be
/// resolved here.
fn thread_conv(conversation: &proto::ConversationIdentifier) -> Option<ConvKey> {
    use proto::conversation_identifier::Identifier;
    let service_id = match conversation.identifier.as_ref()? {
        Identifier::ThreadGroupId(id) => {
            return (id.len() == 32).then(|| groups::conv_key_of_id(id));
        }
        Identifier::ThreadServiceIdBinary(bytes) => service_id_from_wire(Some(bytes), None),
        Identifier::ThreadServiceId(string) => service_id_from_wire(None, Some(string)),
        Identifier::ThreadE164(_) => return None,
    }
    .ok()
    .flatten()?;
    service_id_user_id(service_id).ok().map(ConvKey::Direct)
}

/// The author of an `AddressableMessage`: our own ACI is `Own`.
fn addressable_author(ctx: &Ctx<'_>, message: &proto::AddressableMessage) -> Option<TargetAuthor> {
    use proto::addressable_message::Author;
    let service_id = match message.author.as_ref()? {
        Author::AuthorServiceIdBinary(bytes) => service_id_from_wire(Some(bytes), None),
        Author::AuthorServiceId(string) => service_id_from_wire(None, Some(string)),
        Author::AuthorE164(_) => return None,
    }
    .ok()
    .flatten()?;
    if service_id == ServiceId::Aci(ctx.account.aci) {
        return Some(TargetAuthor::Own);
    }
    service_id_user_id(service_id).ok().map(TargetAuthor::User)
}

/// `SyncMessage.deleteForMe` (`DeletesForMe.preload.ts`, `SD/ts/util/deleteForMe.preload.ts`):
/// what we deleted on another device, for our account only.
///
/// - A message (`messageDeletes`) becomes `Deleted { for_everyone: false }` by our own
///   account; one that has not arrived yet is deleted when it does (the core parks the delete).
/// - A chat (`conversationDeletes`) becomes `ConversationCleared` with the newest messages the
///   deleting device had as anchors (`mostRecentMessages`, then
///   `mostRecentNonExpiringMessages`: Desktop's `deleteConversation` looks for the first set
///   and falls back to the second, the core takes the newest of both it has) and
///   `remove = isFullDelete`. A chat deleted where it had no message to name
///   (`localOnlyConversationDeletes`) has no anchors and `remove` set.
/// - One attachment of a message (`attachmentDeletes`) becomes `AttachmentDeleted`, named by
///   its client UUID and the digest of its blob ([`media::net_id_uuid`],
///   [`media::net_id_digest`]; Desktop's `deleteAttachmentFromMessage` tries them in this
///   order). Desktop's third name, the hash of the plaintext, is not on the wire and nothing
///   here knows it before the file is downloaded, so a delete that carries only that is not
///   applied.
///
/// An entry that names its chat or its message by phone number alone cannot be resolved here
/// and is left out.
fn delete_for_me(ctx: &Ctx<'_>, row: &Row, delete: &proto::sync_message::DeleteForMe) -> Outcome {
    let at = i64::try_from(row.timestamp).unwrap_or(i64::MAX);
    let target = |conv: &ConvKey, message: &proto::AddressableMessage| {
        Some(MsgTarget {
            conv: Some(conv.clone()),
            author: addressable_author(ctx, message)?,
            local: message.sent_timestamp?.to_string().into(),
            alternates: Vec::new(),
        })
    };
    let mut events = Vec::new();
    for deletes in &delete.message_deletes {
        let Some(conv) = deletes.conversation.as_ref().and_then(thread_conv) else {
            continue;
        };
        events.extend(
            deletes
                .messages
                .iter()
                .filter_map(|message| target(&conv, message))
                .map(|target| BackendEvent::Deleted {
                    target,
                    by: None,
                    for_everyone: false,
                    at,
                }),
        );
    }
    for cleared in &delete.conversation_deletes {
        let Some(conv) = cleared.conversation.as_ref().and_then(thread_conv) else {
            continue;
        };
        let up_to = cleared
            .most_recent_messages
            .iter()
            .chain(&cleared.most_recent_non_expiring_messages)
            .filter_map(|message| target(&conv, message))
            .collect();
        events.push(BackendEvent::ConversationCleared {
            conv,
            up_to,
            remove: cleared.is_full_delete.unwrap_or(false),
            at,
        });
    }
    for removed in &delete.local_only_conversation_deletes {
        let Some(conv) = removed.conversation.as_ref().and_then(thread_conv) else {
            continue;
        };
        events.push(BackendEvent::ConversationCleared {
            conv,
            up_to: Vec::new(),
            remove: true,
            at,
        });
    }
    let mut unnamed = 0usize;
    for attachment in &delete.attachment_deletes {
        let Some(target) = attachment
            .conversation
            .as_ref()
            .and_then(thread_conv)
            .zip(attachment.target_message.as_ref())
            .and_then(|(conv, message)| target(&conv, message))
        else {
            continue;
        };
        let net_ids: Vec<String> = attachment
            .client_uuid
            .as_deref()
            .and_then(media::net_id_uuid)
            .into_iter()
            .chain(
                attachment
                    .fallback_digest
                    .as_deref()
                    .and_then(media::net_id_digest),
            )
            .chain(
                attachment
                    .fallback_plaintext_hash
                    .as_deref()
                    .and_then(media::net_id_plaintext_hash),
            )
            .collect();
        if net_ids.is_empty() {
            unnamed += 1;
            continue;
        }
        events.push(BackendEvent::AttachmentDeleted {
            target,
            net_ids: kourier_model::Redacted::new(net_ids),
            at,
        });
    }
    if unnamed > 0 {
        tracing::info!(
            attachments = unnamed,
            "delete for me: attachments without a name are not applied"
        );
    }
    if events.is_empty() {
        Outcome::dropped("delete for me without entries that can be applied")
    } else {
        Outcome::events(events)
    }
}

/// `SyncMessage.read`: we read these messages on another device → one `ReadMarker` each.
fn read_sync(ctx: &Ctx<'_>, read: &[proto::sync_message::Read]) -> Outcome {
    let events: Vec<_> = read
        .iter()
        .filter_map(|entry| {
            let timestamp = entry.timestamp?;
            let author = target_author(
                ctx,
                entry.sender_aci_binary.as_deref(),
                entry.sender_aci.as_deref(),
            )?;
            Some(BackendEvent::ReadMarker {
                conv: None,
                up_to: MsgTarget {
                    conv: None,
                    author,
                    local: timestamp.to_string().into(),
                    alternates: Vec::new(),
                },
            })
        })
        .collect();
    if events.is_empty() {
        Outcome::dropped("read sync without valid entries")
    } else {
        Outcome::events(events)
    }
}

/// `SyncMessage.sent`: a 1:1 message our account sent from another device (Desktop's
/// `onSentMessage`), and its per-recipient state. A transcript with our profile key in it means
/// the phone shares our profile in that chat (`handleDataMessage.preload.ts:686-693`): the chat
/// is accepted. Flags transcripts: a timer change by us, a session reset by us, or a profile-key
/// update (`handleMessageSentProfileUpdate`, which also accepts the chat).
fn sent_transcript(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    sent: &proto::sync_message::Sent,
) -> Result<Outcome> {
    let Some(timestamp) = sent.timestamp else {
        return Ok(Outcome::failed("sent transcript without timestamp", true));
    };
    if let Some(master) = group_dispatch::transcript_group(sent) {
        return group_dispatch::group_transcript(conn, ctx, row, sent, timestamp, master);
    }
    let destination = match service_id_from_wire(
        sent.destination_service_id_binary.as_deref(),
        sent.destination_service_id.as_deref(),
    ) {
        Ok(Some(destination)) => destination,
        _ => {
            return Ok(Outcome::dropped("sent transcript without a destination"));
        }
    };
    let Ok(peer) = service_id_user_id(destination) else {
        return Ok(Outcome::dropped("sent transcript destination id"));
    };
    let conv = ConvKey::Direct(peer.clone());
    let who = Who::Own {
        device: row.sender_device,
    };
    if let Some(edit) = &sent.edit_message {
        // Our own edit is dated by its own timestamp (our clock on the other device).
        let at = i64::try_from(timestamp).unwrap_or(i64::MAX);
        keep_edit_quote(conn, edit, timestamp, ctx.now)?;
        return Ok(edit_events(&who, conv, edit, timestamp, at, ctx.long_text));
    }
    if sent.story_message.is_some() {
        return Ok(Outcome::dropped("story transcript (not in M1)"));
    }
    let state = BackendEvent::OwnSendState {
        local: None,
        target: own_target(Some(conv.clone()), timestamp),
        per_recipient: recipient_states(sent, &peer),
    };
    if sent.is_recipient_update.unwrap_or(false) {
        return Ok(Outcome::events(vec![state]));
    }
    let Some(message) = &sent.message else {
        return Ok(Outcome::dropped("sent transcript without a message"));
    };
    if let Err(reason) = validate(message, timestamp) {
        return Ok(Outcome::failed(reason, true));
    }
    // A reply sent from another device: its quote is kept for the edits Kourier may send.
    outgoing::keep_own_quote(conn, message, timestamp, ctx.now)?;
    let own_chat = destination == ServiceId::from(ctx.account.aci);
    let destination_sid = destination.service_id_string();
    if message.profile_key.is_some() && !own_chat {
        peers::set_profile_sharing(conn, &destination_sid, true, ctx.now)?;
    }
    let flags = message.flags.unwrap_or(0);
    if flags & PROFILE_KEY_UPDATE != 0 {
        if !own_chat {
            peers::set_profile_sharing(conn, &destination_sid, true, ctx.now)?;
        }
        // No timeline entry; the patch carries the chat's new features (`with_features`).
        return Ok(Outcome::events(vec![BackendEvent::ConversationUpsert(
            ConversationPatch::new(conv),
        )]));
    }
    let at = i64::try_from(timestamp).unwrap_or(i64::MAX);
    let outcome = if flags & EXPIRATION_TIMER_UPDATE != 0 {
        let row = Row {
            timestamp,
            ..row.clone()
        };
        match destination {
            ServiceId::Aci(_) => timer_update(
                conn,
                ctx,
                &row,
                &destination_sid,
                &who,
                conv,
                message,
                timestamp,
            )?,
            ServiceId::Pni(_) => Outcome::dropped("timer update to a PNI"),
        }
    } else if flags & END_SESSION != 0 {
        // We reset the session from another device: our sessions with them are stale too.
        archive_all_sessions(conn, &destination_sid, ctx.now)?;
        let row = Row {
            timestamp,
            ..row.clone()
        };
        Outcome::events(vec![system_message(
            ctx,
            &row,
            &who,
            conv,
            SystemEvent::SessionRefreshed { user: peer.clone() },
        )])
    } else if flags != 0 {
        return Ok(Outcome::dropped("transcript with unknown flags"));
    } else {
        let outcome = content_events(ctx, row, &who, conv.clone(), message, timestamp, at);
        match destination {
            // The timer our phone sent the message with is the chat's (same rules as for a
            // message of the peer).
            ServiceId::Aci(_) => with_stated_timer(
                conn,
                ctx,
                row,
                &destination_sid,
                &who,
                &conv,
                message,
                timestamp,
                outcome,
            )?,
            ServiceId::Pni(_) => outcome,
        }
    };
    Ok(match outcome {
        Outcome::Handled {
            mut events,
            ephemeral,
            note,
        } => {
            let is_message = events.iter().any(
                |e| matches!(e, BackendEvent::Message(m) if !matches!(m.body, Body::System(_))),
            );
            for event in &mut events {
                if let BackendEvent::Message(own) = event
                    && let Some(expiry) = &mut own.expiry
                {
                    // `expirationStartTimestamp`: when our other device started the timer
                    // (Desktop: `Math.min(expirationStartTimestamp || timestamp, now)`).
                    expiry.started_at = Some(own_timer_start(sent, timestamp, ctx.now));
                }
            }
            if is_message {
                events.push(state);
            }
            Outcome::Handled {
                events,
                ephemeral,
                note,
            }
        }
        failed @ Outcome::Failed { .. } => failed,
    })
}

/// Keeps the quote an edit of ours from another device carries, under the edit's own id (the
/// next edit targets this revision; [`outgoing::keep_own_quote`]).
pub(super) fn keep_edit_quote(
    conn: &Connection,
    edit: &proto::EditMessage,
    timestamp: u64,
    now: i64,
) -> Result<()> {
    match &edit.data_message {
        Some(message) => outgoing::keep_own_quote(conn, message, timestamp, now),
        None => Ok(()),
    }
}

/// When the timer of our own message from another device started (`onSentMessage`): the
/// transcript's `expirationStartTimestamp`, else the message's sent time, never in the future.
pub(super) fn own_timer_start(sent: &proto::sync_message::Sent, timestamp: u64, now: i64) -> i64 {
    sent.expiration_start_timestamp
        .filter(|start| *start > 0)
        .or(Some(timestamp))
        .and_then(|start| i64::try_from(start).ok())
        .unwrap_or(now)
        .min(now)
}

/// The per-recipient states of a sent transcript (`unidentifiedStatus`); the peer itself when
/// the list is empty.
pub(super) fn recipient_states(
    sent: &proto::sync_message::Sent,
    peer: &UserId,
) -> Vec<(UserId, RecipientState)> {
    let states: Vec<_> = sent
        .unidentified_status
        .iter()
        .filter_map(|status| {
            let recipient = service_id_from_wire(
                status.destination_service_id_binary.as_deref(),
                status.destination_service_id.as_deref(),
            )
            .ok()
            .flatten()?;
            let user = service_id_user_id(recipient).ok()?;
            Some((
                user,
                RecipientState::Sent {
                    sealed: status.unidentified.unwrap_or(false),
                },
            ))
        })
        .collect();
    if states.is_empty() {
        vec![(peer.clone(), RecipientState::Sent { sealed: false })]
    } else {
        states
    }
}

/// `SyncMessage.messageRequestResponse` for a 1:1 chat: accepted or blocked on another device.
/// Accepting turns profile sharing on, blocking off (`applyMessageRequestResponse`).
fn message_request_response(
    conn: &Connection,
    ctx: &Ctx<'_>,
    response: &proto::sync_message::MessageRequestResponse,
) -> Result<Outcome> {
    use proto::sync_message::message_request_response::Type;
    if let Some(group_id) = response.group_id.as_deref() {
        return match response.r#type.and_then(|t| Type::try_from(t).ok()) {
            Some(Type::Accept) => {
                group_dispatch::group_request_response(conn, ctx, group_id, false)
            }
            Some(Type::Block | Type::BlockAndSpam | Type::BlockAndDelete) => {
                group_dispatch::group_request_response(conn, ctx, group_id, true)
            }
            _ => Ok(Outcome::dropped(
                "group message request delete or spam (no event for it)",
            )),
        };
    }
    let Some(aci) = aci_from_wire(
        response.thread_aci_binary.as_deref(),
        response.thread_aci.as_deref(),
    )
    .ok()
    .flatten() else {
        return Ok(Outcome::dropped(
            "message request response without a thread",
        ));
    };
    let Ok(peer) = aci_user_id(aci) else {
        return Ok(Outcome::dropped(
            "message request response without a thread",
        ));
    };
    let conv = ConvKey::Direct(peer.clone());
    let mut patch = ConversationPatch::new(conv);
    let blocked = match response.r#type.and_then(|t| Type::try_from(t).ok()) {
        Some(Type::Accept) => false,
        Some(Type::Block | Type::BlockAndSpam | Type::BlockAndDelete) => true,
        Some(Type::Delete | Type::Spam | Type::Unknown) | None => {
            return Ok(Outcome::dropped(
                "message request delete or spam (no event for it)",
            ));
        }
    };
    peers::set_profile_sharing(
        conn,
        &ServiceId::from(aci).service_id_string(),
        !blocked,
        ctx.now,
    )?;
    peers::set_blocked(
        conn,
        &ServiceId::from(aci).service_id_string(),
        blocked,
        ctx.now,
    )?;
    patch.request_pending = Some(false);
    patch.blocked = Some(blocked);
    let mut contact = ContactPatch::new(peer);
    contact.blocked = Some(blocked);
    Ok(Outcome::events(vec![
        BackendEvent::ContactUpsert(contact),
        BackendEvent::ConversationUpsert(patch),
    ]))
}

/// `ReceiptMessage` (`#handleReceiptMessage`): one target per timestamp, all our own messages.
/// A delivery receipt also settles the Message Send Log rows of that device.
fn receipt_message(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    sender: Option<ServiceId>,
    receipt: &proto::ReceiptMessage,
) -> Result<Outcome> {
    use proto::receipt_message::Type;
    let Some(ServiceId::Aci(aci)) = sender else {
        return Ok(Outcome::dropped("receipt from a non-ACI sender"));
    };
    let Ok(by) = aci_user_id(aci) else {
        return Ok(Outcome::dropped("sender id"));
    };
    let kind = match receipt.r#type.map(Type::try_from) {
        Some(Ok(Type::Delivery)) | None => ReceiptKind::Delivered,
        Some(Ok(Type::Read)) => ReceiptKind::Read,
        Some(Ok(Type::Viewed)) => ReceiptKind::Viewed,
        Some(Err(_)) => return Ok(Outcome::dropped("receipt of an unknown type")),
    };
    if receipt.timestamp.is_empty() {
        return Ok(Outcome::dropped("receipt without timestamps"));
    }
    if kind == ReceiptKind::Delivered {
        delivered(conn, ctx, aci.into(), row.sender_device, &receipt.timestamp)?;
    }
    Ok(Outcome::events(vec![BackendEvent::Receipts {
        targets: receipt
            .timestamp
            .iter()
            .map(|t| own_target(None, *t))
            .collect(),
        by,
        kind,
        at: i64::try_from(row.timestamp).unwrap_or(i64::MAX),
    }]))
}

/// `TypingMessage` (`#handleTypingMessage`, item 25): ephemeral, never stored; stale indicators
/// from the catch-up backlog are dropped, and all of them while the typing setting is off
/// (Desktop's `onTyping`).
fn typing_message(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    sender: Option<ServiceId>,
    typing: &proto::TypingMessage,
) -> Result<Outcome> {
    use proto::typing_message::Action;
    let Some(ServiceId::Aci(aci)) = sender else {
        return Ok(Outcome::dropped("typing from a non-ACI sender"));
    };
    if aci == ctx.account.aci {
        return Ok(Outcome::dropped("typing from our own device"));
    }
    if typing.timestamp != Some(row.timestamp) {
        return Ok(Outcome::dropped(
            "typing timestamp differs from the envelope",
        ));
    }
    if typing.group_id.is_some() {
        let Ok(user) = aci_user_id(aci) else {
            return Ok(Outcome::dropped("sender id"));
        };
        return group_dispatch::group_typing(conn, ctx, row, aci, user, typing);
    }
    if ctx.catch_up {
        return Ok(Outcome::dropped("typing from the catch-up backlog"));
    }
    if !db::synced_settings(conn)?.typing_indicators {
        return Ok(Outcome::dropped("typing indicators are off"));
    }
    let Ok(user) = aci_user_id(aci) else {
        return Ok(Outcome::dropped("sender id"));
    };
    let on = !matches!(
        typing.action.map(Action::try_from),
        Some(Ok(Action::Stopped))
    );
    Ok(Outcome::Handled {
        events: Vec::new(),
        ephemeral: vec![BackendEvent::Typing {
            conv: ConvKey::Direct(user.clone()),
            by: user,
            device: Some(row.sender_device),
            on,
            ttl: TYPING_TTL,
        }],
        note: None,
    })
}

/// A reference to our own message sent at `timestamp`.
pub(super) fn own_target(conv: Option<ConvKey>, timestamp: u64) -> MsgTarget {
    MsgTarget {
        conv,
        author: TargetAuthor::Own,
        local: timestamp.to_string().into(),
        alternates: Vec::new(),
    }
}

/// The placeholder a dropped poison row leaves in the timeline (§5.6): an undecryptable message
/// with the would-be id (sender, timestamp) in the sender's 1:1 chat; `None` when the sender is
/// not an ACI, or is our own account (a sync message from another device of ours has no such
/// chat: "a message from me in my note-to-self" would mislead).
pub(crate) fn poison_placeholder(row: &Row, own: Aci, catch_up: bool) -> Option<BackendEvent> {
    let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(&row.sender) else {
        return None;
    };
    if aci == own {
        return None;
    }
    let author = aci_user_id(aci).ok()?;
    let mut message = IncomingMessage::new(
        ConvKey::Direct(author.clone()),
        row.timestamp.to_string(),
        Origin::Remote {
            author,
            device: Some(row.sender_device),
        },
        Body::Placeholder(Placeholder::Undecryptable { resendable: false }),
        i64::try_from(row.timestamp).unwrap_or(i64::MAX),
        row.received_at,
    );
    message.server_at = row.server_timestamp;
    message.arrival = Some(row.arrival);
    message.catch_up = catch_up;
    Some(BackendEvent::Message(message))
}
