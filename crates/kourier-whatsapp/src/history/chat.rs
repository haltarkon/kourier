//! The history map, chats (W4, C10): what a chunk says of a CHAT besides its messages. PURE, like
//! [`super::map`]: no I/O, no clock, no logging.
//!
//! A chat of a chunk becomes one entry of a chats row ([`Chat::events`]):
//! 1. `IdentityMerged { from: number, into: lid }` when the chat states both forms of its person
//!    (`pnJid` and `lidJid`, or an `id` in one form beside the other). It comes FIRST: the chat's
//!    messages are keyed by the LID ([`super::map::Place`]), and a chat the live path made under the
//!    number joins the LID chat only through the merge, so the chunk's pairs are stated before any batch
//!    of the chat. The pair is vouched by our own phone; a pair that names one of OUR ids is never
//!    stated (the core skips those too, and a number we are is nobody's alias).
//! 2. `ConversationUpsert`: a group's title, the disappearing timer, a community's parent, and the unread
//!    hint. NOT stated, on purpose: archived, pinned, muted and marked-unread (the phone's app state
//!    carries them, W6, with an order a chunk cannot give), the members and roles (W6 reads the group),
//!    the features (the live path states none before W6) and a direct chat's `name` (it can be an
//!    address-book name, which is no title).
//!
//! LOCKED CHATS are delivered like any other (his "Show like any chat", D1): `locked` is not read.
//!
//! THE UNREAD HINT (D6, O2). WhatsApp states a chat's unread COUNT and never which messages are unread, so
//! the history rows say `read: true` and the chat states the count as `unread_hint`. The core REPLACES a
//! hint with the next one and clears it when the chat is read here, so the hint is stated at MOST ONCE per
//! conversation ([`hint`]) and never after the chat was read on this computer: a second chunk naming the
//! chat, or a take-back redo, must not bring the badge back. A live message the phone also counts (it
//! arrived after the link but is older than the chat's last message at the phone) already is a row the
//! core counts, so it is subtracted. The decision belongs to the delivery zone (it reads `history_chats`
//! in the zone that appends the row), which is why a [`Chat`] keeps the hint apart until it is asked for
//! its events.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, ConversationPatch, Patch, UserId};
use wacore_binary::{Jid, Server};
use waproto::whatsapp as wa;

use super::map::{self, Place, Target};
use crate::map::{Ctx, OwnIds, conv_of, scheme_of, vouched_pair};
use crate::notice::TITLE_CAP;
use crate::send::started;

/// The version a timer gets when the phone states it without the time it was set (the oldest there is): a
/// change the live path saw wins over it, and it wins over nothing.
const OLDEST_TIMER_VERSION: u32 = 1;

/// What a chunk says of one chat, mapped. Built by [`chat`].
#[derive(Debug, Clone)]
pub(crate) struct Chat {
    /// The conversation the chat is (`history_chats.conv` is its encoded form).
    pub(crate) conv: ConvKey,
    /// The phone's count of unread messages, when the chunk carries one: the CANDIDATE for the hint.
    pub(crate) unread: Option<u32>,
    /// The time (ms) the phone's count stands at: a live message of someone else at or before it is
    /// inside the count. Unknown (the chat states no time): everything, so a busy chat is under-counted
    /// rather than shown unread twice.
    pub(crate) live_cut_ms: i64,
    /// The phone says it has nothing more to send for this chat (`endOfHistoryTransfer`).
    pub(crate) ended: bool,
    pair: Option<(UserId, UserId)>,
    patch: ConversationPatch,
    /// The chat's trusted-contact token, when the phone states one (W5a, D12): written by the delivery zone.
    pub(crate) token: Option<Token>,
    /// The chunk carries at least one REAL message of this direct chat, from either side
    /// ([`crate::send::started::proves_start`]: not a call record, not a notice): the only thing in a chunk that proves
    /// the chat was started on the phone (W5a, fix F2). A chats row alone (a time, an unread count, a name, a
    /// flag, a token) proves nothing.
    pub(crate) real: bool,
}

/// A one-to-one chat's trusted-contact token as a chunk states it (`Conversation.tcToken*`), with the user its row in
/// `tc_tokens` is keyed by when the chunk itself names the LID (upstream's key rule: the LID user when known, else the
/// number's user; [`Token::key`]). Its `Debug` prints no byte and no id.
#[derive(Clone)]
pub(crate) struct Token {
    /// The user of the chat's id (`Conversation.id`), the number's or the LID's.
    pub(crate) user: String,
    /// The LID user the chunk states for the chat (its id when that is a LID, else its `lidJid`).
    pub(crate) lid: Option<String>,
    pub(crate) bytes: Vec<u8>,
    /// `tcTokenTimestamp` (s).
    pub(crate) at: i64,
    /// `tcTokenSenderTimestamp` (s), when stated.
    pub(crate) sender_at: Option<i64>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// The token of a conversation, as upstream's history extraction takes it (`extract_conversation_fields`): only a
/// one-to-one chat (not a group, a channel or a bot) whose id reads, with a non-empty token and its time.
pub(crate) fn token_of(conversation: &wa::Conversation) -> Option<Token> {
    let bytes = conversation.tc_token.as_deref().filter(|b| !b.is_empty())?;
    let at = i64::try_from(conversation.tc_token_timestamp?).ok()?;
    let id: Jid = conversation.id.parse().ok()?;
    if id.user.is_empty() || !matches!(scheme_of(id.server), Some("pn" | "lid")) {
        return None;
    }
    let lid = if scheme_of(id.server) == Some("lid") {
        Some(id.user.to_string())
    } else {
        conversation
            .lid_jid
            .as_deref()
            .and_then(|text| text.parse::<Jid>().ok())
            .filter(|jid| scheme_of(jid.server) == Some("lid") && !jid.user.is_empty())
            .map(|jid| jid.user.to_string())
    };
    Some(Token {
        user: id.user.to_string(),
        lid,
        bytes: bytes.to_vec(),
        at,
        sender_at: conversation
            .tc_token_sender_timestamp
            .and_then(|t| i64::try_from(t).ok()),
    })
}

impl Chat {
    /// The events of the chat's entry in a chats row: its pair first, then the upsert, which carries
    /// `hint` (see [`hint`]) as `unread_hint`.
    pub(crate) fn events(&self, hint: Option<u32>) -> Vec<BackendEvent> {
        let mut events = Vec::with_capacity(2);
        if let Some((from, into)) = self.pair.clone() {
            events.push(BackendEvent::IdentityMerged { from, into });
        }
        let mut patch = self.patch.clone();
        patch.unread_hint = hint;
        events.push(BackendEvent::ConversationUpsert(patch));
        events
    }
}

/// What the backend knows of a chat's hint (its `history_chats` row; both false when it has none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HintState {
    /// A hint was stated for this chat before.
    pub(crate) stated: bool,
    /// The chat was read on this computer since the link.
    pub(crate) read_here: bool,
}

/// The `unread_hint` to state for a chat: the phone's count `phone` minus the `live` messages of others the
/// live path delivered that the count includes, ONCE per chat and never after the chat was read here. `None`:
/// state nothing (and do not mark the chat as stated: a later chunk may bring a real count). The caller
/// marks the chat stated exactly when this returns `Some`.
pub(crate) fn hint(phone: Option<u32>, live: u32, state: HintState) -> Option<u32> {
    if state.stated || state.read_here {
        return None;
    }
    let count = phone?.saturating_sub(live);
    (count > 0).then_some(count)
}

/// Maps a chat of a chunk placed at `place`. `None` for a chat that gives no event: a status chat, a chat this
/// build has no conversation for, and a one-to-one chat in which nothing happened (no message, no time, no
/// unread count: the phone lists such an entry for a contact it never talked to, and it is no chat).
pub(crate) fn chat(place: &Place, conversation: &wa::Conversation, ctx: &Ctx) -> Option<Chat> {
    let Target::Conv(conv) = &place.target else {
        return None;
    };
    let direct = matches!(conv, ConvKey::Direct(_));
    let unread = conversation.unread_count;
    let last_ms = conversation
        .last_msg_timestamp
        .or(conversation.conversation_timestamp)
        .filter(|seconds| *seconds > 0);
    if direct && conversation.messages.is_empty() && last_ms.is_none() && unread.unwrap_or(0) == 0 {
        return None;
    }

    let mut patch = ConversationPatch::new(conv.clone());
    if !direct {
        patch.title = title_of(conversation);
        patch.parent = parent_of(conversation);
    }
    (patch.timer, patch.timer_version) = timer_of(conversation);
    let real = direct && carries_a_real_message(place, conversation, ctx);
    Some(Chat {
        conv: conv.clone(),
        unread,
        live_cut_ms: last_ms
            .and_then(|seconds| i64::try_from(seconds).ok())
            .and_then(|seconds| seconds.checked_mul(1000))
            .unwrap_or(i64::MAX),
        ended: conversation.end_of_history_transfer == Some(true),
        pair: direct.then(|| pair_of(conversation, &ctx.own)).flatten(),
        patch,
        token: if direct { token_of(conversation) } else { None },
        real,
    })
}

/// Whether the chat's messages in the chunk hold one that proves the chat was started on the phone: the map the rows
/// use (`super::map::message`) says it is a message, and [`started::proves_start`] says a person wrote it (fix F2).
/// Stops at the first one.
fn carries_a_real_message(place: &Place, conversation: &wa::Conversation, ctx: &Ctx) -> bool {
    conversation
        .messages
        .iter()
        .filter_map(|entry| entry.message.as_option())
        .any(|info| {
            // The time only stamps `received_at`: it decides nothing here.
            matches!(
                map::message(place, info, ctx, 0),
                map::Item::Message(message) if started::proves_start(&message)
            )
        })
}

/// A group's name. An empty one changes nothing (WhatsApp has no unnamed group).
fn title_of(conversation: &wa::Conversation) -> Patch<String> {
    match conversation.name.as_deref() {
        Some(name) if !name.trim().is_empty() => Patch::Set(name.chars().take(TITLE_CAP).collect()),
        _ => Patch::Unchanged,
    }
}

/// A community's parent: a group id in `parentGroupId`.
fn parent_of(conversation: &wa::Conversation) -> Patch<ConvKey> {
    conversation
        .parent_group_id
        .as_deref()
        .and_then(|text| text.parse::<Jid>().ok())
        .filter(|jid| jid.server == Server::Group)
        .and_then(|jid| conv_of(&jid))
        .map_or(Patch::Unchanged, Patch::Set)
}

/// The chat's disappearing timer and the version that orders it against a change the live path saw.
/// A timer the phone states WITH the time it was set carries that time (the live path's own version); one
/// without it carries the oldest version, so it never overrides a change that came live. "Off" is stated
/// only with its time: without one it cannot be told from a chat that never had a timer.
fn timer_of(conversation: &wa::Conversation) -> (Patch<Duration>, Option<u32>) {
    let version = conversation
        .ephemeral_setting_timestamp
        .and_then(|seconds| u32::try_from(seconds).ok())
        .filter(|version| *version > 0);
    match (conversation.ephemeral_expiration, version) {
        (Some(seconds), version) if seconds > 0 => (
            Patch::Set(Duration::from_secs(u64::from(seconds))),
            Some(version.unwrap_or(OLDEST_TIMER_VERSION)),
        ),
        (Some(0), Some(version)) => (Patch::Clear, Some(version)),
        _ => (Patch::Unchanged, None),
    }
}

/// `(number, lid)` of the person of a one-to-one chat when the chat states both forms (its `id`, `lidJid` and
/// `pnJid` are the candidates): [`vouched_pair`], so never a pair that names one of our ids.
fn pair_of(conversation: &wa::Conversation, own: &OwnIds) -> Option<(UserId, UserId)> {
    let forms: Vec<Jid> = [
        Some(conversation.id.as_str()),
        conversation.lid_jid.as_deref(),
        conversation.pn_jid.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(|text| text.parse().ok())
    .collect();
    let of = |scheme: &str| {
        forms
            .iter()
            .find(|jid| scheme_of(jid.server) == Some(scheme))
    };
    vouched_pair(of("pn")?, of("lid")?, own)
}

#[cfg(test)]
mod tests;
