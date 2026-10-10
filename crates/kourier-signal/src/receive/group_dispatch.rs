//! Stage-2 mapping of group content (§6.6, §6.13; `SD/ts/messages/handleDataMessage.preload.ts`
//! 300-400, `MessageReceiver.preload.ts` `#handleTypingMessage`): messages, edits, our phone's
//! transcripts and typing in a group, after the lane step applied the group's newest state.
//!
//! Drop rules (R:signal-desktop-receive §9 item 20): the group must be known and not blocked,
//! we must be a full member, the sender too (after the update the message implied), the group
//! must not be terminated, and in an announcement-only group only administrators' messages
//! show. Timer changes and session resets travel as group changes, so those `DataMessage` flags
//! are dropped in groups.

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use kourier_model::{ConversationPatch, UserId};
use libsignal_core::Aci;

use super::calls;
use super::dispatch::{
    Ctx, END_SESSION, EXPIRATION_TIMER_UPDATE, Outcome, PROFILE_KEY_UPDATE, TYPING_TTL, Who,
    content_events, edit_events, edit_time, keep_edit_quote, own_target, own_timer_start, validate,
};
use super::stage2::Row;
use crate::db;
use crate::error::Result;
use crate::groups::{self, GroupKeys, GroupRow, Membership, Snapshot};
use crate::proto;
use crate::service_id::service_id_from_wire;

/// The stored group of a `groupV2.masterKey`, if any.
fn group_of(conn: &Connection, master_key: &[u8]) -> Result<Option<GroupRow>> {
    if GroupKeys::from_slice(master_key).is_none() {
        return Ok(None);
    }
    groups::load_by_master(conn, master_key)
}

/// The snapshot a message from `sender` may be shown under, or why it is dropped.
fn admit(group: Option<&GroupRow>, sender: Option<Aci>) -> Result<&Snapshot, &'static str> {
    let Some(group) = group else {
        return Err("group message for an unknown group");
    };
    let Some(snapshot) = group.snapshot.as_ref() else {
        return Err("group message for a group we could not read");
    };
    if group.blocked {
        return Err("message in a blocked group");
    }
    if group.membership != Membership::Member {
        return Err("message in a group we are not a member of");
    }
    if snapshot.terminated {
        return Err("message in a terminated group");
    }
    if let Some(sender) = sender
        && !snapshot.is_member(sender)
    {
        return Err("group message from a non-member");
    }
    Ok(snapshot)
}

/// The group a call signal of `member` (a ring, its cancellation) is shown in, or why it is
/// not shown: the rules of a group message ([`admit`]: a known, readable group that is not
/// blocked or ended, we are a full member and so is `member`), and in an announcement-only
/// group only an administrator's (Desktop's `handleGroupCallRingUpdate` writes nothing for
/// any of these).
pub(super) fn admit_call(group: Option<&GroupRow>, member: Aci) -> Result<GroupKeys, &'static str> {
    let snapshot = admit(group, Some(member))?;
    if snapshot.announcements_only && !snapshot.member(member).is_some_and(|m| m.admin) {
        return Err("call signal of a non-admin in an announcement-only group");
    }
    group
        .map(|g| g.keys)
        .ok_or("group message for an unknown group")
}

/// Whether a group message from `sender` is a timeline message an announcement-only group
/// refuses (reactions and deletes still apply).
fn refused_by_announcements(
    snapshot: &Snapshot,
    sender: Aci,
    message: &proto::DataMessage,
) -> bool {
    snapshot.announcements_only
        && message.reaction.is_none()
        && message.delete.is_none()
        && message.admin_delete.is_none()
        && !snapshot.member(sender).is_some_and(|m| m.admin)
}

/// A group `DataMessage` from another user (`aci`, already checked as not blocked).
pub(super) fn group_message(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    user: UserId,
    message: &proto::DataMessage,
) -> Result<Outcome> {
    let Some(master) = message
        .group_v2
        .as_ref()
        .and_then(|g| g.master_key.as_deref())
    else {
        return Ok(Outcome::dropped("group context without a master key"));
    };
    let group = group_of(conn, master)?;
    let snapshot = match admit(group.as_ref(), Some(aci)) {
        Ok(snapshot) => snapshot,
        Err(note) => return Ok(Outcome::dropped(note)),
    };
    let flags = message.flags.unwrap_or(0);
    if flags & (END_SESSION | EXPIRATION_TIMER_UPDATE | PROFILE_KEY_UPDATE) != 0 {
        return Ok(Outcome::dropped(
            "group message flags (timers and resets are group changes)",
        ));
    }
    if refused_by_announcements(snapshot, aci, message) {
        return Ok(Outcome::dropped(
            "non-admin message in an announcement-only group",
        ));
    }
    let Some(keys) = group.as_ref().map(|g| g.keys) else {
        return Ok(Outcome::dropped("group message for an unknown group"));
    };
    let who = Who::Remote {
        user,
        device: row.sender_device,
    };
    if let Some(update) = &message.group_call_update {
        // Somebody started or joined a call: a signal of that call's line, and nothing else
        // of the message is read (Desktop consumes such a message whole).
        return Ok(calls::group_call_update(
            ctx,
            row,
            &who,
            keys.conv_key(),
            update,
            row.timestamp,
        ));
    }
    let at = row
        .server_timestamp
        .unwrap_or_else(|| i64::try_from(row.timestamp).unwrap_or(i64::MAX));
    Ok(content_events(
        ctx,
        row,
        &who,
        keys.conv_key(),
        message,
        row.timestamp,
        at,
    ))
}

/// Whether delivery receipts are owed for a sealed group message: we are a full member (the
/// group is accepted; Desktop's `getAccepted` also asks for profile sharing, which M1 cannot
/// write back).
pub(super) fn delivery_receipts_owed(
    conn: &Connection,
    message: &proto::DataMessage,
) -> Result<bool> {
    let Some(master) = message
        .group_v2
        .as_ref()
        .and_then(|g| g.master_key.as_deref())
    else {
        return Ok(false);
    };
    Ok(group_of(conn, master)?.is_some_and(|g| g.membership == Membership::Member && !g.blocked))
}

/// A group `EditMessage` from another user.
pub(super) fn group_edit(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    user: UserId,
    edit: &proto::EditMessage,
) -> Result<Outcome> {
    let Some(message) = edit.data_message.as_ref() else {
        return Ok(Outcome::failed("edit without target or message", true));
    };
    let Some(master) = message
        .group_v2
        .as_ref()
        .and_then(|g| g.master_key.as_deref())
    else {
        return Ok(Outcome::dropped("group context without a master key"));
    };
    let group = group_of(conn, master)?;
    let snapshot = match admit(group.as_ref(), Some(aci)) {
        Ok(snapshot) => snapshot,
        Err(note) => return Ok(Outcome::dropped(note)),
    };
    if refused_by_announcements(snapshot, aci, message) {
        return Ok(Outcome::dropped(
            "non-admin edit in an announcement-only group",
        ));
    }
    let Some(keys) = group.as_ref().map(|g| g.keys) else {
        return Ok(Outcome::dropped("group message for an unknown group"));
    };
    let who = Who::Remote {
        user,
        device: row.sender_device,
    };
    Ok(edit_events(
        &who,
        keys.conv_key(),
        edit,
        row.timestamp,
        edit_time(row, row.timestamp),
        ctx.long_text,
    ))
}

/// The group of a sent transcript, when it is a group message (or edit).
pub(super) fn transcript_group(sent: &proto::sync_message::Sent) -> Option<&[u8]> {
    sent.message
        .as_ref()
        .or(sent
            .edit_message
            .as_ref()
            .and_then(|e| e.data_message.as_ref()))?
        .group_v2
        .as_ref()?
        .master_key
        .as_deref()
}

/// Our phone's transcript of a group message (or edit) sent at `timestamp`: an own message in
/// the group with the per-recipient states the phone reports.
pub(super) fn group_transcript(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    sent: &proto::sync_message::Sent,
    timestamp: u64,
    master: &[u8],
) -> Result<Outcome> {
    let group = group_of(conn, master)?;
    if let Err(note) = admit(group.as_ref(), None) {
        return Ok(Outcome::dropped(note));
    }
    let Some(keys) = group.as_ref().map(|g| g.keys) else {
        return Ok(Outcome::dropped("group message for an unknown group"));
    };
    let conv = keys.conv_key();
    let who = Who::Own {
        device: row.sender_device,
    };
    if let Some(edit) = &sent.edit_message {
        let at = i64::try_from(timestamp).unwrap_or(i64::MAX);
        keep_edit_quote(conn, edit, timestamp, ctx.now)?;
        return Ok(edit_events(&who, conv, edit, timestamp, at, ctx.long_text));
    }
    let Some(message) = &sent.message else {
        return Ok(Outcome::dropped("sent transcript without a message"));
    };
    if let Err(reason) = validate(message, timestamp) {
        return Ok(Outcome::failed(reason, true));
    }
    // A reply sent from another device: its quote is kept for the edits Kourier may send.
    crate::send::outgoing::keep_own_quote(conn, message, timestamp, ctx.now)?;
    if message.flags.unwrap_or(0) != 0 {
        return Ok(Outcome::dropped(
            "group transcript flags (timers and resets are group changes)",
        ));
    }
    if let Some(update) = &message.group_call_update {
        // The user joined or started the call on another device: the call's line, answered.
        // No send state: a call line is no message of ours.
        return Ok(calls::group_call_update(
            ctx, row, &who, conv, update, timestamp,
        ));
    }
    let state = BackendEvent::OwnSendState {
        local: None,
        target: own_target(Some(conv.clone()), timestamp),
        per_recipient: sent
            .unidentified_status
            .iter()
            .filter_map(|status| {
                let recipient = service_id_from_wire(
                    status.destination_service_id_binary.as_deref(),
                    status.destination_service_id.as_deref(),
                )
                .ok()
                .flatten()?;
                let user = crate::service_id::service_id_user_id(recipient).ok()?;
                Some((
                    user,
                    kourier_model::RecipientState::Sent {
                        sealed: status.unidentified.unwrap_or(false),
                    },
                ))
            })
            .collect(),
    };
    if sent.is_recipient_update.unwrap_or(false) {
        return Ok(Outcome::events(vec![state]));
    }
    let at = i64::try_from(timestamp).unwrap_or(i64::MAX);
    Ok(
        match content_events(ctx, row, &who, conv, message, timestamp, at) {
            Outcome::Handled {
                mut events,
                ephemeral,
                note,
            } => {
                let is_message = events.iter().any(|e| matches!(e, BackendEvent::Message(_)));
                for event in &mut events {
                    if let BackendEvent::Message(own) = event
                        && let Some(expiry) = &mut own.expiry
                    {
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
        },
    )
}

/// A group `TypingMessage` (item 25): a known group we are in, from a member, live, with the
/// typing setting on.
pub(super) fn group_typing(
    conn: &Connection,
    ctx: &Ctx<'_>,
    row: &Row,
    aci: Aci,
    user: UserId,
    typing: &proto::TypingMessage,
) -> Result<Outcome> {
    use proto::typing_message::Action;
    let Some(group_id) = typing.group_id.as_deref() else {
        return Ok(Outcome::dropped("typing without a group"));
    };
    let group = groups::load_by_id(conn, group_id)?;
    if let Err(note) = admit(group.as_ref(), Some(aci)) {
        return Ok(Outcome::dropped(note));
    }
    let Some(keys) = group.as_ref().map(|g| g.keys) else {
        return Ok(Outcome::dropped("typing in an unknown group"));
    };
    if ctx.catch_up {
        return Ok(Outcome::dropped("typing from the catch-up backlog"));
    }
    if !db::synced_settings(conn)?.typing_indicators {
        return Ok(Outcome::dropped("typing indicators are off"));
    }
    let on = !matches!(
        typing.action.map(Action::try_from),
        Some(Ok(Action::Stopped))
    );
    Ok(Outcome::Handled {
        events: Vec::new(),
        ephemeral: vec![BackendEvent::Typing {
            conv: keys.conv_key(),
            by: user,
            device: Some(row.sender_device),
            on,
            ttl: TYPING_TTL,
        }],
        note: None,
    })
}

/// `SyncMessage.messageRequestResponse` for a group: accepted or blocked on our phone.
pub(super) fn group_request_response(
    conn: &Connection,
    ctx: &Ctx<'_>,
    group_id: &[u8],
    blocked: bool,
) -> Result<Outcome> {
    let Some(group) = groups::load_by_id(conn, group_id)? else {
        return Ok(Outcome::dropped("request response for an unknown group"));
    };
    conn.prepare_cached(
        "UPDATE groups SET blocked = ?2, whitelisted = ?3, updated_at = ?4 WHERE group_id = ?1",
    )?
    .execute((group_id, blocked, !blocked, ctx.now))?;
    if group.features_hash.is_none() {
        return Ok(Outcome::dropped(
            "request response for a group the core lacks",
        ));
    }
    let mut patch = ConversationPatch::new(group.keys.conv_key());
    patch.blocked = Some(blocked);
    patch.request_pending = Some(false);
    Ok(Outcome::events(vec![BackendEvent::ConversationUpsert(
        patch,
    )]))
}

/// The groups of a `SyncMessage.blocked` list: those we know are blocked (their chats updated
/// when the core has them). Groups the list no longer names stay as they are: the Storage
/// Service is the source of truth for unblocking (§6.9).
pub(super) fn blocked_groups(
    conn: &Connection,
    ctx: &Ctx<'_>,
    group_ids: &[Vec<u8>],
) -> Result<Vec<BackendEvent>> {
    let mut events = Vec::new();
    for id in group_ids {
        let Some(group) = groups::load_by_id(conn, id)? else {
            continue;
        };
        if group.blocked {
            continue;
        }
        conn.prepare_cached("UPDATE groups SET blocked = 1, updated_at = ?2 WHERE group_id = ?1")?
            .execute((&id[..], ctx.now))?;
        if group.features_hash.is_some() {
            let mut patch = ConversationPatch::new(group.keys.conv_key());
            patch.blocked = Some(true);
            events.push(BackendEvent::ConversationUpsert(patch));
        }
    }
    Ok(events)
}
