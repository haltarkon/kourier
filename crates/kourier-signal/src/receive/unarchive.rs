//! A new message brings an archived chat back (A1, archived chats can be opened).
//!
//! Signal's rule, as Desktop (`canConversationBeUnarchived.preload.ts`) and Android
//! (`ThreadTable.allowedToUnarchive`) keep it: a chat the phone archived comes back to the
//! inbox when a message arrives, a muted one too, unless the phone's "Keep muted chats
//! archived" is on and the chat is muted at that moment. The rule lives here, in the Signal
//! backend, and nowhere else: the core takes any `ConversationUpsert { archived }` and
//! WhatsApp's "keep chats archived" differs. Kourier's own "mute on this computer" is core-only
//! state the backend never sees, so it cannot keep a chat archived (an e2e case holds that).
//!
//! What counts is decided on the events one inbox row produced ([`bring_back`], called by
//! [`super::dispatch::dispatch`] after the features of the chats), which is why a reaction,
//! an edit, a delete, a receipt, a read marker, typing, a profile or group-state update, a
//! timer change and every history-import row (a `HistoryBatch`, never this path) leave a chat
//! archived without a line for each: only a `BackendEvent::Message` is looked at, and of
//! those only
//!
//! - a message somebody else wrote, unless it says it was read elsewhere first (Desktop's
//!   read-sync branch comes first), and not a stand-in (`Body::Placeholder`) or a notice
//!   (`Body::System`: group changes, timer changes, safety number, session refresh, number
//!   change, request accepted, joined);
//! - a message of our account from another device, a sync transcript (Desktop runs the phone's
//!   transcripts through the same path as its own sends);
//! - every call line, whoever's signal and whatever it says (Desktop saves each call-history
//!   entry through the same unarchive).
//!
//! A group's story reply does not count (Desktop leaves it out); a 1:1 one does. Kourier's own
//! sends bring a chat back from [`after_own_send`], which `send/` calls once the server has
//! accepted a new message (Desktop unarchives before the send; here the chat stays where it is
//! while the message cannot go out).
//!
//! The chat's base moves with the unarchive, in the same zone: `peers.roaming_archived` for a
//! 1:1 chat or Note to Self, `groups.archived` for a group. Without it the Storage merge
//! would compare the phone's next record, which says archived again, with the record before
//! it, which said so too, and never archive the chat again (the critic's B1). The stored
//! record itself is never rewritten (A10's write engine edits COPIES of the stored bytes, and a
//! click on Archive that it has not written yet is dropped by the same zone: `leaves_the_archive`
//! calls `storage::intents::chat_left_the_archive`, critic X1). The lookup is two column reads of rows the receive path
//! has at hand (a peer's row, a group's flags), never the stored records, except once for a
//! peer whose base was never written (a profile from before migration 0023): that one reads
//! the peer's newest stored record, offline, and keeps the answer ([`storage::fill_roaming`]).
//! That answer is the phone's, not what the core was told, so the first counting message of such
//! a peer states `archived: false` whatever the filled base says, when a stored record gave it:
//! one idempotent upsert per peer in the profile's life. (Known limit: after Link Again a chat the phone archived while
//! the account was logged out stays in the inbox until the phone's next record of it.)

use std::rc::Rc;

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::Tx;
use kourier_db::rusqlite::Connection;
use kourier_model::{Body, ConvKey, ConversationPatch, Origin, SystemEvent};
use tokio::sync::Notify;

use super::dispatch::Ctx;
use crate::db::Account;
use crate::error::Result;
use crate::{features, groups, outbox, peers, storage};

/// `Body::Unsupported::kind_hint` of a story reply (`dispatch.rs` `message_body`).
const STORY_REPLY: &str = "story-reply";

/// `events` with, in front of the first message of an archived chat that comes back, the
/// upsert that says so (the chat is in the core before the message lands in it), and the
/// chat's base moved on `conn`, the zone of the row. Rows without a counting message are
/// returned as they are, with no read at all.
pub(crate) fn bring_back(
    conn: &Connection,
    ctx: &Ctx<'_>,
    events: Vec<BackendEvent>,
) -> Result<Vec<BackendEvent>> {
    if !events.iter().any(counts) {
        return Ok(events);
    }
    let mut out = Vec::with_capacity(events.len() + 1);
    for event in events {
        if let BackendEvent::Message(message) = &event
            && message_counts(message)
            && leaves_the_archive(conn, ctx.account, ctx.now, &message.conv)?
        {
            out.push(unarchive_upsert(&message.conv));
        }
        out.push(event);
    }
    Ok(out)
}

/// A new message of ours that the server accepted brings its chat back, like Desktop's own send
/// does (`beforeMessageSend/unarchive`, the same rule as for an incoming message): the upsert
/// goes to the core's outbox and the chat's base moves, in the one zone `tx`. Nothing happens
/// for a chat that is not archived (two column reads), and a send that is tried again after the
/// zone committed finds the base moved and writes nothing.
pub(crate) fn after_own_send(
    tx: &mut Tx<'_>,
    account: &Account,
    now: i64,
    conv: &ConvKey,
    wake: &Rc<Notify>,
) -> Result<()> {
    if leaves_the_archive(tx, account, now, conv)? {
        outbox::append_events(tx, &[unarchive_upsert(conv)], now, wake)?;
    }
    Ok(())
}

fn unarchive_upsert(conv: &ConvKey) -> BackendEvent {
    let mut patch = ConversationPatch::new(conv.clone());
    patch.archived = Some(false);
    BackendEvent::ConversationUpsert(patch)
}

fn counts(event: &BackendEvent) -> bool {
    matches!(event, BackendEvent::Message(message) if message_counts(message))
}

/// Whether this message brings its chat back, by the message alone (the module doc).
fn message_counts(message: &IncomingMessage) -> bool {
    match &message.body {
        Body::System(SystemEvent::Call(_)) => true,
        Body::System(_) | Body::Placeholder(_) => false,
        Body::Unsupported { kind_hint, .. }
            if kind_hint == STORY_REPLY && matches!(message.conv, ConvKey::Group(_)) =>
        {
            false
        }
        _ => match message.origin {
            Origin::Remote { .. } => !message.read,
            Origin::OwnOtherDevice { .. } => true,
            // Brought back once the server took it ([`after_own_send`], called by `send/`).
            Origin::OwnThisDevice => false,
        },
    }
}

/// Whether the chat is archived and comes back now; when it does the base is moved here.
fn leaves_the_archive(
    conn: &Connection,
    account: &Account,
    now: i64,
    conv: &ConvKey,
) -> Result<bool> {
    match conv {
        ConvKey::Direct(user) => {
            // A person known only by a number has no peers row and no roaming state.
            let Some(peer) = features::direct_service_id(account, user) else {
                return Ok(false);
            };
            let sid = peer.service_id_string();
            // `told`: the base was filled from a stored record of the phone (not what the core
            // was told).
            let (base, told) = match peers::load(conn, &sid)?.roaming {
                Some(base) => (base, false),
                None => storage::fill_roaming(conn, account, &sid, now)?,
            };
            if base.archived {
                if stays_archived(conn, base.muted_until, now)? {
                    return Ok(false);
                }
                peers::set_roaming_archived(conn, &sid, false, now)?;
                // A10 (critic X1): an Archive click the phone has not heard yet is overtaken by
                // the message, as the phone's own copy of this message overtakes it.
                storage::intents::chat_left_the_archive(
                    conn,
                    &storage::intents::direct_target(account, &sid),
                    now,
                )?;
                return Ok(true);
            }
            // A base filled from a record is what the phone says, not what the core was told (a
            // core that kept the chat archived across Link Again, whose fresh backend.db stated
            // nothing of its first records): say "not archived" once. The fill wrote the base as
            // it is, `archived = false`. A peer with no record at all is a chat nobody archived
            // anywhere: nothing to heal, and no extra upsert in front of every new contact's
            // first message.
            Ok(told)
        }
        ConvKey::Group(_) => {
            let Some(id) = groups::id_of_conv(conv) else {
                return Ok(false);
            };
            let Some((archived, muted_until)) = groups::archive_state(conn, &id)? else {
                return Ok(false);
            };
            if !archived || stays_archived(conn, muted_until, now)? {
                return Ok(false);
            }
            groups::set_archived(conn, &id, false, now)?;
            if let Some(master) = groups::master_of(conn, &id)? {
                storage::intents::chat_left_the_archive(
                    conn,
                    &storage::group_target(&master),
                    now,
                )?;
            }
            Ok(true)
        }
        // Signal has no other kind of chat.
        ConvKey::Broadcast(_) | ConvKey::Channel(_) => Ok(false),
    }
}

/// The one exception: the phone mutes the chat NOW (`until` in ms, `i64::MAX` for ever) and
/// its "Keep muted chats archived" is on (off until an account record said otherwise).
fn stays_archived(conn: &Connection, muted_until: Option<i64>, now: i64) -> Result<bool> {
    if !muted_until.is_some_and(|until| until > now) {
        return Ok(false);
    }
    storage::keep_muted_chats_archived(conn)
}
