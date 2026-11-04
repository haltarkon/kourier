//! What sits beside a message's content (W3b, chunk C4a): the people it mentions and the text they and
//! WhatsApp's formatting make of it (3.3), the message it quotes (3.4), and the pairs of ids the stanza
//! vouches for (3.2). PURE: no I/O, no clock, no logging.
//!
//! A person is emitted AS THE STANZA NAMES HIM (`lid:` or `pn:`); the core's alias machinery does the rest
//! once a stanza vouches for the pair ([`merges`]).

use kourier_backend_api::BackendEvent;
use kourier_model::ids::validate_local;
use kourier_model::{ConvKey, MsgTarget, QuoteSnapshot, ReplyTo, TargetAuthor, UserId};
use wacore::proto_helpers::MessageExt as _;
use wacore_binary::Jid;
use waproto::whatsapp as wa;

use super::class;
use super::{Ctx, Row, person_of, vouched_pair};
use crate::inbox::InboxMeta;
use crate::markup::{self, Marked, Mentioned};
use crate::media;

/// Most mentioned people read from one message (`markup` takes the same number).
const MAX_MENTIONED: usize = 1_000;

/// The context info of a (peeled) message: the first of the kinds that carry one. It holds the quote, the
/// mentions and the message's own timer.
pub(super) fn info_of(base: &wa::Message) -> Option<&wa::ContextInfo> {
    base.context_info()
}

/// The people a context mentions, in the order it lists them. A JID that is not a person (a bot, a
/// community) is dropped: its `@name` stays plain text. `me` is set for one of our own ids, a LID or a number.
fn mentioned(info: Option<&wa::ContextInfo>, ctx: &Ctx) -> Vec<Mentioned> {
    let Some(info) = info else {
        return Vec::new();
    };
    info.mentioned_jid
        .iter()
        .filter_map(|text| {
            let jid: Jid = text.parse().ok()?;
            let user = person_of(&jid)?;
            Some(Mentioned {
                handle: jid.user.to_string(),
                me: ctx.own.is_own(&user),
                user,
            })
        })
        .take(MAX_MENTIONED)
        .collect()
}

/// A message's text through WhatsApp's formatting, with the people its context info mentions.
pub(super) fn marked(text: &str, info: Option<&wa::ContextInfo>, ctx: &Ctx) -> Marked {
    markup::mark_up(text, &mentioned(info, ctx))
}

/// Who a quote or a key names, from the `participant` the stanza states.
///
/// - one of our own ids: `Own` (the quote's `author` field says "own or unknown" with `None`);
/// - another person, as the stanza names him (`lid:` or `pn:`): `User`;
/// - none stated, or a participant that is no person: `Unknown`, which the core resolves by `local`
///   inside the conversation. A 1:1 quote without a participant could be of either side; a wrong guess
///   would leave the quote unresolved, `Unknown` finds it in both cases (random ids do not collide).
pub(super) fn quoted_author(
    participant: Option<&str>,
    ctx: &Ctx,
) -> (TargetAuthor, Option<UserId>) {
    let user = participant
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<Jid>().ok())
        .and_then(|jid| person_of(&jid));
    match user {
        Some(user) if ctx.own.is_own(&user) => (TargetAuthor::Own, None),
        Some(user) => (TargetAuthor::User(user.clone()), Some(user)),
        None => (TargetAuthor::Unknown, None),
    }
}

/// The message a context info quotes: `stanzaId` names it, `participant` its author, `quotedMessage` is the
/// sender's copy, which becomes the snapshot.
///
/// The CONVERSATION of the target is THIS row's, always. `remoteJid` is the chat as the SENDER sees it (in
/// a direct chat that is us, a conversation that does not exist here) or the status feed a reply answers
/// (nothing of it is kept: D1); neither is a conversation of ours, and WhatsApp does not quote across
/// chats. A quote of a status keeps its snapshot and a target that will not resolve (K-B3 draws it from the
/// snapshot). A `stanzaId` that is not a valid message id gives no quote (the message still maps).
pub(super) fn reply_to(
    conv: &ConvKey,
    info: Option<&wa::ContextInfo>,
    ctx: &Ctx,
) -> Option<ReplyTo> {
    let info = info?;
    let local = info
        .stanza_id
        .as_deref()
        .filter(|id| validate_local(id).is_ok())?;
    let (author, quoted) = quoted_author(info.participant.as_deref(), ctx);
    Some(ReplyTo {
        target: MsgTarget {
            conv: Some(conv.clone()),
            author,
            local: local.into(),
            alternates: Vec::new(),
        },
        author: quoted,
        snapshot: snapshot(info.quoted_message.as_option()),
    })
}

/// The sender's copy of the quoted message: its text (WhatsApp's formatting read, no mentions: the quoted
/// message's own context is not ours to trust), the kind, name and type of its first file.
fn snapshot(quoted: Option<&wa::Message>) -> Option<QuoteSnapshot> {
    let base = class::peel(quoted?);
    let text = base
        .text_content()
        .filter(|t| !t.is_empty())
        .or_else(|| base.get_caption().filter(|t| !t.is_empty()))
        .map(|t| markup::mark_up(t, &[]).text);
    let file = media::glimpse(base);
    Some(QuoteSnapshot {
        text,
        attachment: file.as_ref().map(|f| f.kind),
        file_name: file.as_ref().and_then(|f| f.file_name.clone()),
        mime: file.and_then(|f| f.mime),
        thumb: None,
    })
}

/// A pair of ids the stanza vouches for, as `IdentityMerged { from: pn, into: lid }`: [`vouched_pair`] of two
/// ids given as text.
fn pair(a: &str, b: &str, ctx: &Ctx) -> Option<(UserId, UserId)> {
    let a: Jid = a.parse().ok()?;
    let b: Jid = b.parse().ok()?;
    vouched_pair(&a, &b, &ctx.own)
}

/// The `IdentityMerged` events a row's stanza vouches for, first in the row's events (3.2): the sender with
/// his other identity (`sender_alt`), and, for a message of ours only, the LID chat's peer with the number
/// the stanza gave for him (`recipient_alt`: upstream sets it only for our own self-synced direct messages,
/// so a row that is not ours never vouches a pair with it). Never for our own ids; the same pair once.
pub(super) fn merges(row: &Row, meta: &InboxMeta, ctx: &Ctx) -> Vec<BackendEvent> {
    let mut pairs: Vec<(UserId, UserId)> = Vec::new();
    let mut vouch = |a: &str, b: Option<&String>| {
        if let Some(found) = b.and_then(|b| pair(a, b, ctx))
            && !pairs.contains(&found)
        {
            pairs.push(found);
        }
    };
    vouch(&row.sender, meta.sender_alt.as_ref());
    if meta.from_me {
        vouch(&row.chat, meta.recipient_alt.as_ref());
    }
    pairs
        .into_iter()
        .map(|(from, into)| BackendEvent::IdentityMerged { from, into })
        .collect()
}
