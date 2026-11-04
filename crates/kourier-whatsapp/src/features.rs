//! The features of a WhatsApp chat (architecture §3.8 tier 3; W5a, brief §2.8, decisions D7 and D9), and where the
//! backend states them.
//!
//! # Limits, and who enforces each
//!
//! WhatsApp's own apps and the receivers' code are the reference for the protocol, not for their limits: a limit is
//! stated only where the server or the other clients enforce it, or where it protects the account. `U` names an
//! open question of the brief's §4 (checked in the real send run, R2).
//!
//! - **Text**: `max_text_bytes` 65,536 [U]: the core splits a longer text into several messages
//!   (`kourier-core` `outbox.rs`), so nothing is refused for its length.
//! - **Formatting**: bold, italic, strikethrough and monospace, which every WhatsApp client draws from the same
//!   markers (`markup`'s inverse writes them). A mention ONLY IN GROUPS (critic L4): WhatsApp's mentions are a
//!   group feature; a 1:1 chat offers none. No spoiler, no "mention everyone", no link with another text.
//! - **Reply**: a quote of any message of the chat (`Full`).
//! - **Edit**: own messages within 15 minutes, as WhatsApp's apps offer it (`Window{max_age_secs: 900}`; the
//!   receivers accept 1,200 s, upstream `msg_secret.rs` `EDIT_PROCESSING_WINDOW_SECS`, U4). The core measures the
//!   window from the server's time; the backend's slack rule (`send::attempt`, D9) refuses an edit the receivers
//!   would drop when it waited too long. No count limit.
//! - **Received edits and deletes** (fix F16): the core DROPS a received edit or delete-for-everyone when the chat's
//!   `edit_receive` / `delete_receive` is `None`, so both are stated. An edit is taken within 1,200 s of its original
//!   (`edit_receive`; the receivers drop a later one, upstream `msg_secret.rs`, so Kourier shows what the phones
//!   show); a delete has no stated age limit (the receivers' window for a revoke is [U]). In WhatsApp's own chat
//!   ("Message yourself") the core lets an edit of any age through (Signal's note-to-self rule): a core item, named in
//!   architecture §13.
//! - **Delete for everyone**: own messages within 60 hours, as WhatsApp's apps offer it (`216_000 s`, U4). Delete for
//!   me is W6's (an app-state mutation), so it is off; one that still arrives is answered `Sent` with no frame
//!   and logged as a fault (`send::attempt`, critic L6).
//! - **Reactions**: one per person per message, a new one replaces the old (`reactions_per_user: Some(1)`).
//! - **Typing, read and delivery receipts**: yes (the phone's read-receipt setting acts through upstream, D18).
//! - **Files** (W5b, G6, G11; `send::route`): pictures, videos, GIFs, audio and any other file, each up to
//!   [`FILE_LIMIT`] (2,000,000,000 bytes, below WhatsApp's own 2 GiB constant with room [U2]; the backend's route
//!   ADAPTS what WhatsApp's media messages cannot carry instead of refusing it: a picture, video or audio file over
//!   `MEDIA_LIMIT` (16,000,000 bytes) or of a format the phones do not play inline goes as a document, a GIF always
//!   does). A caption on every kind but audio (an `audioMessage` carries none; the core sends an audio file's caption
//!   as a text of its own). One file per message (`file_packing` default: no albums, G10). Voice notes and stickers
//!   are not sent (G9: the composer makes neither).
//! - **Disappearing timers** (W6a2, B7b, D22): a DIRECT chat offers the three timers WhatsApp's apps offer, 24 hours,
//!   7 days and 90 days, counted from the send (`AfterSend`; [`TIMER_CHOICES`]), and no custom range (the apps ignore
//!   another value); Kourier sets one with a frame of its own (`send::timer`). A message sent INTO a timer chat carries
//!   the chat's timer (`send::content`). A group states none: its timer is an admin's group change (K8c).
//! - **History**: pushed after the link, as the descriptor says.
//! - **Admin delete and the group's rules** (W6a2, D10, D11): a group states `admin_delete` (the core then offers an admin
//!   Delete for Everyone on another member's message, and the backend builds the admin revoke, chunk B7b), `group_info`
//!   (only admins change the group, only admins add members, only admins send, ended) and `cannot_send` (`AdminsOnly`
//!   while only admins may send and none of our ids is one, `NotMember` after we left or were removed; never
//!   `StillLoading`). They come from the group's row (`groups::features`), stated with the members in the same patch
//!   and again whenever a notice or a fetch changes them; a direct chat states none of it. Group actions (adding
//!   members, changing the group) are K8c.
//! - **Who may send** (`cannot_send`): a DIRECT chat the phone never started states `NotAccepted` ("start this
//!   chat on your phone", his rule D6: a companion's first message into a chat nobody started is refused by the
//!   server and counts against the account); `None` from the moment the started fact holds (`send::started`).
//!   A group is always started (its membership is the server's); admin-only and left groups are W6's, and their
//!   nack is a `Failed` with its code until then (`send::errors`).
//!
//! # Where they are stated
//!
//! A chat is stated once per ([`VERSION`], started fact) (`features_stated` in `backend.db`, migration 5), in the
//! zone that writes the events it rides with ([`with_statements`]): stage 2's (every message row, a group's
//! notice), the event writer's (a call line, a notice), W4's delivery zone (a chats row of the history). The
//! statement is a `ConversationUpsert` of its own, carrying only the features, right before the first event that
//! names the chat, after the started fact of the same zone was noted: the first message of the peer lifts
//! `NotAccepted` in the row that carries it. At every start that is not a fresh link the [`sweep`] states what was
//! never stated or is stale: the chats of the history, and every chat stated before with another version or
//! another fact; a `Rebuild` of the core forgets every statement first, in the zone that states `AccountReady`
//! again (critic M4), so the sweep states every chat again. A chat with no event since and no history row keeps
//! the features it has (none after a link: the send path works without them, and the backend's own refusals,
//! D7 and the slack rule, do not depend on them).

use std::collections::BTreeMap;

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, Transaction};
use kourier_model::{
    AttachmentKind, CannotSend, ConvKey, ConversationFeatures, ConversationPatch, DisappearKind,
    FileLimits, FormatKind, HistorySource, Support, Window,
};
use wacore::store::error::StoreError;

use crate::groups;
use crate::send::route::FILE_LIMIT;
use crate::send::started;
use crate::store::{people, rows};

/// The version of what [`of`] states: raised whenever it states something else, so every chat is stated again at
/// the next start ([`sweep`]) or with its next event.
pub(crate) const VERSION: u32 = 4;

/// The disappearing timers of a direct chat that WhatsApp's apps offer (seconds): 24 hours, 7 days, 90 days. The apps ignore
/// any other value (whatsmeow `send.go:650-654`), so none is set or stated.
pub(crate) const TIMER_CHOICES: [u32; 3] = [86_400, 604_800, 7_776_000];

/// How long WhatsApp's apps offer to edit an own message (15 minutes).
const EDIT_WINDOW_SECS: u32 = 900;
/// How long after its original the receivers take an edit: 20 minutes (upstream `EDIT_PROCESSING_WINDOW_SECS`, WA Web's
/// AB prop; `message/msg_secret.rs` drops an edit authored 1,200 s or more after its parent). Stated as `edit_receive`
/// so the core drops a late edit as the phones do (fix F16).
const EDIT_RECEIVE_WINDOW_SECS: u32 = 1200;
/// How long WhatsApp's apps offer to delete an own message for everyone (60 hours).
const DELETE_WINDOW_SECS: u32 = 216_000;
/// The longest text one message carries before the core splits it [U].
const MAX_TEXT_BYTES: u32 = 65_536;
/// Statements per `core_outbox` row of the start sweep.
const SWEEP_ROW: usize = 100;

/// The features of `conv`: a direct chat or a group (the backend makes no other kind), `started` the started fact
/// of `send::started::holds`.
pub(crate) fn of(conv: &ConvKey, started: bool) -> ConversationFeatures {
    let group = matches!(conv, ConvKey::Group(_));
    let mut formatting = BTreeMap::new();
    for kind in [
        FormatKind::Bold,
        FormatKind::Italic,
        FormatKind::Strike,
        FormatKind::Mono,
    ] {
        formatting.insert(kind, Support::Full);
    }
    if group {
        formatting.insert(FormatKind::Mention, Support::Full);
    }
    let files = [
        AttachmentKind::Image,
        AttachmentKind::Video,
        AttachmentKind::Gif,
        AttachmentKind::Audio,
        AttachmentKind::File,
    ]
    .into_iter()
    .map(|kind| {
        (
            kind,
            FileLimits {
                max_size: FILE_LIMIT,
                caption: kind != AttachmentKind::Audio,
            },
        )
    })
    .collect();
    ConversationFeatures {
        max_text_bytes: Some(MAX_TEXT_BYTES),
        formatting,
        files,
        reply: Support::Full,
        edit: Some(Window {
            max_age_secs: Some(EDIT_WINDOW_SECS),
            max_count: None,
        }),
        delete_for_everyone: Some(Window {
            max_age_secs: Some(DELETE_WINDOW_SECS),
            max_count: None,
        }),
        delete_for_me: false,
        edit_receive: Some(Window {
            max_age_secs: Some(EDIT_RECEIVE_WINDOW_SECS),
            max_count: None,
        }),
        // The core refuses a received delete when this is `None` (`ingest/modifier.rs`); the receivers' window for a
        // revoke is [U], so no age limit is stated (a delete is the author's own act on his own message).
        delete_receive: Some(Window {
            max_age_secs: None,
            max_count: None,
        }),
        // A group's admin may delete another member's message for everyone (W6a2, D11): the receive side maps it and the
        // send side builds the admin revoke (B7b). A direct chat has no admin.
        admin_delete: group,
        reactions: Support::Full,
        reactions_per_user: Some(1),
        // A direct chat's timer is set by a frame of Kourier's own (W6a2 B7b); a group's is K8c's.
        disappearing: if group {
            BTreeMap::new()
        } else {
            BTreeMap::from([(DisappearKind::AfterSend, TIMER_CHOICES.to_vec())])
        },
        typing: true,
        read_receipts: true,
        delivery_receipts: true,
        history: HistorySource::PushedOnLink,
        cannot_send: (!started).then_some(CannotSend::NotAccepted),
        ..ConversationFeatures::default()
    }
}

/// The statement of `conv`'s features: an upsert that carries the features and nothing else. A group's are made from the
/// row of what the core was told of it ([`groups::features`]: the rules and our place the last fetch or notice said), so a
/// statement made again (a new [`VERSION`], a `Rebuild`) never takes `group_info` or `cannot_send` back (W6a2, M4).
fn statement(conn: &Connection, conv: &ConvKey, started: bool) -> Result<BackendEvent, StoreError> {
    let mut patch = ConversationPatch::new(conv.clone());
    patch.features = Some(match conv {
        ConvKey::Group(_) => groups::features(conv, &people::group(conn, conv)?),
        _ => of(conv, started),
    });
    Ok(BackendEvent::ConversationUpsert(patch))
}

/// Whether `event` is a statement of this module (an upsert with features and nothing else): a history row taken
/// back forgets it (`history::settle`), and test readers that compare a row's other events leave it out.
pub(crate) fn is_statement(event: &BackendEvent) -> bool {
    match event {
        BackendEvent::ConversationUpsert(patch) if patch.features.is_some() => {
            let mut bare = ConversationPatch::new(patch.key.clone());
            bare.features.clone_from(&patch.features);
            *patch == bare
        }
        _ => false,
    }
}

/// The chat an event names for a statement: a message (a call line and a notice are messages too) or an upsert of
/// a direct chat or a group. Receipts, typing and the rest state nothing.
fn named(event: &BackendEvent) -> Option<&ConvKey> {
    let conv = match event {
        BackendEvent::Message(message) => &message.conv,
        BackendEvent::ConversationUpsert(patch) => &patch.key,
        _ => return None,
    };
    matches!(conv, ConvKey::Direct(_) | ConvKey::Group(_)).then_some(conv)
}

/// Whether one of `events` names a chat a statement may be due for ([`with_statements`] has work to do).
pub(crate) fn names_a_chat(events: &[BackendEvent]) -> bool {
    events.iter().any(|event| named(event).is_some())
}

/// States `conv` when it needs it (never stated, or with another version or fact): marks it stated in this zone
/// and gives the statement, followed by what the phone's app state said about the chat before the core knew it (W6a1,
/// H5: `appstate::zone::settings_of`; nothing for a chat stated before). Empty when nothing is due.
pub(crate) fn state(tx: &Transaction<'_>, conv: &ConvKey) -> Result<Vec<BackendEvent>, StoreError> {
    let started = started::holds(tx, conv)?;
    if !started::needs_statement(tx, conv, VERSION, started)? {
        return Ok(Vec::new());
    }
    started::mark_stated(tx, conv, VERSION, started)?;
    // A person the core now has a chat with is known to it, and what he is owed goes with the statement: the pairs and the
    // push name a history chunk kept of him (W6a2 F11) and the phone's address-book name (if the mirror holds one the core was
    // not told: B8b, the scope rule of D8). A merge goes in front of the chat's statement, as the live path orders it.
    let owed = match conv {
        ConvKey::Direct(user) => crate::book::states(tx, std::slice::from_ref(user))?,
        _ => Vec::new(),
    };
    let merges = owed
        .iter()
        .take_while(|event| matches!(event, BackendEvent::IdentityMerged { .. }))
        .count();
    let mut events = owed[..merges].to_vec();
    events.push(statement(tx, conv, started)?);
    events.extend(crate::appstate::zone::settings_of(tx, conv)?);
    events.extend_from_slice(&owed[merges..]);
    Ok(events)
}

/// `events` with the statements they need, in the zone that writes them: each right before the first event that
/// names its chat. The caller notes the started facts of the same events FIRST. `None` when nothing needed a
/// statement (the caller writes `events` as they are).
pub(crate) fn with_statements(
    tx: &Transaction<'_>,
    events: &[BackendEvent],
) -> Result<Option<Vec<BackendEvent>>, StoreError> {
    // The pairs of a number and a LID these events state are recorded for the merge sweep (W6a2, B9).
    crate::merges::record(tx, events)?;
    let mut seen: Vec<&ConvKey> = Vec::new();
    let mut out: Option<Vec<BackendEvent>> = None;
    for (i, event) in events.iter().enumerate() {
        if let Some(conv) = named(event)
            && !seen.contains(&conv)
        {
            seen.push(conv);
            let stated = state(tx, conv)?;
            if !stated.is_empty() {
                out.get_or_insert_with(|| events[..i].to_vec())
                    .extend(stated);
            }
        }
        if let Some(out) = &mut out {
            out.push(event.clone());
        }
    }
    // The people these events name (a direct chat's peer, a list's members, a message's author) become known to the core, and
    // the address-book names the mirror holds for them are stated with the same events (W6a2, B8b).
    if !names_a_chat(events) {
        return Ok(out);
    }
    let named = crate::book::with_names(tx, out.as_deref().unwrap_or(events))?;
    Ok(named.or(out))
}

/// The start sweep: the statements of the chats of the history and of every chat stated before, wherever one is
/// missing or stale, in rows of at most [`SWEEP_ROW`] (marked stated in this zone; the caller appends the rows in
/// it too).
pub(crate) fn sweep(tx: &Transaction<'_>) -> Result<Vec<Vec<BackendEvent>>, StoreError> {
    let keys = rows(
        tx,
        "read the chats whose features may be stale",
        "SELECT conv FROM history_chats UNION SELECT conv FROM features_stated ORDER BY 1",
        [],
        |r| r.get::<_, String>(0),
    )?;
    let mut statements = Vec::new();
    for key in keys {
        let Ok(conv) = ConvKey::parse(&key) else {
            continue;
        };
        if !matches!(conv, ConvKey::Direct(_) | ConvKey::Group(_)) {
            continue;
        }
        statements.extend(state(tx, &conv)?);
    }
    Ok(statements
        .chunks(SWEEP_ROW)
        .map(<[BackendEvent]>::to_vec)
        .collect())
}

#[cfg(test)]
mod tests;
