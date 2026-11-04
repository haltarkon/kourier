//! The message secrets of a history row (W4, C13).
//!
//! A message's 32-byte `messageSecret` is the key its later poll votes, event replies, encrypted reactions
//! and edits are opened with. The phone puts it into the history chunk; upstream seeds it into the store
//! from the chunk when it has finished the chunk (after the receipt, detached: a kill there loses it). The
//! delivery writes the secrets of a row's messages in the SAME zone that appends the row, so the secret is
//! in the store exactly when the message is in the core, whatever kills the process in between.
//!
//! The KEY of a secret is the one upstream's own seeding uses, so the two never disagree and a lookup finds
//! a row whichever wrote it: the chat is the chunk's `Conversation.id`, the sender follows the rule below
//! (`group_participant_raw`, `history_msg_secret_senders`: ADAPTED from whatsapp-rust, MIT, rev 50a723d4,
//! `src/history_sync.rs`; the holder is named in `REUSE.toml`). What is not taken: a FORWARDED message's
//! secret (it belongs to the original message; upstream skips it, as WhatsApp Web does) and a secret that is
//! not 32 bytes.
//!
//! The retention is "never" (`expires_at = 0`): the client is built with `MsgSecretPolicy::Full`
//! ([`crate::session::MSG_SECRET_POLICY`], pinned by a test), which keeps a secret as long as its message.
//!
//! PURE: no I/O, no clock, no logging; a secret is never printed.

use std::sync::Arc;

use wacore::proto_helpers::MessageExt as _;
use wacore::store::traits::MsgSecretEntry;
use wacore_binary::{Jid, JidExt as _};
use waproto::whatsapp as wa;

/// A secret is this long (`reporting_token::MESSAGE_SECRET_SIZE` upstream).
const SECRET_SIZE: usize = 32;

/// At most two aliases are stored for one secret: the account's number and LID for a message of ours.
const MAX_SENDERS: usize = 2;
type Senders = [Option<Jid>; MAX_SENDERS];

/// The fields of a history entry the sender rule reads.
#[derive(Clone, Copy)]
struct Record<'a> {
    from_me: bool,
    key_participant: Option<&'a str>,
    web_msg_participant: Option<&'a str>,
}

/// The secret entries of one message of the chat `chat_id` (the chunk's `Conversation.id`), or none: no key or
/// id, a forwarded message, no secret of the right size, a chat id that does not parse, or a sender nobody
/// can name. `own_pn` and `own_lid` are our account's ids (a message of ours is keyed by both).
pub(crate) fn entries(
    chat_id: &str,
    info: &wa::WebMessageInfo,
    own_pn: Option<&Jid>,
    own_lid: Option<&Jid>,
) -> Vec<MsgSecretEntry> {
    let Some(key) = info.key.as_option() else {
        return Vec::new();
    };
    let Some(msg_id) = key.id.as_deref().filter(|id| !id.is_empty()) else {
        return Vec::new();
    };
    let message = info.message.as_option();
    if message.is_some_and(|message| message.is_forwarded()) {
        return Vec::new();
    }
    let Some(secret) = info
        .message_secret
        .as_deref()
        .or_else(|| message.and_then(context_secret))
    else {
        return Vec::new();
    };
    let Ok(secret) = <[u8; SECRET_SIZE]>::try_from(secret) else {
        return Vec::new();
    };
    let Ok(chat) = chat_id.parse::<Jid>() else {
        return Vec::new();
    };
    let record = Record {
        from_me: key.from_me.unwrap_or(false),
        key_participant: key.participant.as_deref(),
        web_msg_participant: info.participant.as_deref(),
    };
    let chat_arc: Arc<str> = Arc::from(chat.to_non_ad_string());
    let msg_id: Arc<str> = Arc::from(msg_id);
    let message_ts = info
        .message_timestamp
        .and_then(|seconds| i64::try_from(seconds).ok())
        .unwrap_or(0);
    senders(&chat, record, own_pn, own_lid)
        .into_iter()
        .flatten()
        .map(|sender| MsgSecretEntry {
            chat: Arc::clone(&chat_arc),
            sender: MsgSecretEntry::sender_id_for(&chat, &chat_arc, &sender),
            msg_id: Arc::clone(&msg_id),
            secret,
            expires_at: 0,
            message_ts,
        })
        .collect()
}

/// The secret in the message's own `MessageContextInfo` (where a message the phone wrapped carries it).
fn context_secret(message: &wa::Message) -> Option<&[u8]> {
    message
        .message_context_info
        .as_option()?
        .message_secret
        .as_deref()
}

/// The raw participant field the entry's sender is read from, or `None` when the sender comes from our own
/// identity (`fromMe`) or from the chat itself (a one-to-one chat, a bot).
fn group_participant_raw<'a>(chat: &Jid, record: Record<'a>) -> Option<&'a str> {
    if record.from_me || chat.is_pn() || chat.is_lid() || chat.is_bot() {
        return None;
    }
    record.key_participant.or(record.web_msg_participant)
}

fn senders(chat: &Jid, record: Record<'_>, own_pn: Option<&Jid>, own_lid: Option<&Jid>) -> Senders {
    let mut senders: Senders = std::array::from_fn(|_| None);

    if record.from_me {
        if let Some(lid) = own_lid {
            push_unique(&mut senders, lid.to_non_ad());
        }
        if let Some(pn) = own_pn {
            push_unique(&mut senders, pn.to_non_ad());
        }
        return senders;
    }

    if chat.is_pn() || chat.is_lid() || chat.is_bot() {
        push_unique(&mut senders, chat.to_non_ad());
        if chat.is_bot()
            && let Some(lid) = own_lid
        {
            push_unique(&mut senders, lid.to_non_ad());
        }
        return senders;
    }

    if let Some(raw_sender) = group_participant_raw(chat, record)
        && let Ok(sender) = raw_sender.parse::<Jid>()
    {
        push_unique(&mut senders, sender.to_non_ad());
    }

    senders
}

fn push_unique(senders: &mut Senders, sender: Jid) {
    if senders.iter().flatten().any(|existing| existing == &sender) {
        return;
    }
    if let Some(slot) = senders.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(sender);
    }
}

#[cfg(test)]
mod tests;
