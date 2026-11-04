//! The modifiers (W3b, chunk C5): a reaction, an edit, a revoke (delete for everyone) and the chat's timer
//! setting. PURE: no I/O, no clock, no logging.
//!
//! WHO WROTE THE TARGET (critic H1, lead decision D4). A message key is written from the point of view of
//! the modifier's SENDER (whatsmeow `msgsecret.go` `getOrigSenderFromKey`, the same rule as its history
//! import): `key.fromMe` says the target is the sender's own; else in a 1:1 chat `key.remoteJid` is the chat
//! AS THE SENDER SEES IT (us when a peer reacts to our message), never the conversation; in a group
//! `key.participant` names the author. The conversation of a target is always THIS row's. So, for the four
//! cases of a 1:1 chat:
//!
//! | modifier sent by | `key.fromMe` | target written by |
//! |---|---|---|
//! | the peer | true | the peer |
//! | the peer | false | us (`Own`) |
//! | our phone | true | us (`Own`) |
//! | our phone | false | the peer |
//!
//! and in a group: `fromMe` the sender, else the `participant` (`Own` when it is one of our ids, `Unknown`
//! when none is stated: the core then finds the target by its id inside the conversation).
//!
//! Times are the SERVER's (the stanza's `t`, critic M3): the sender's own `timestampMs` is his to choose, and
//! the core measures the edit and delete windows from the server's time.

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::ids::validate_local;
use kourier_model::{
    Body, ConvKey, ConversationPatch, MsgTarget, Patch, SystemEvent, TargetAuthor, UserId,
};
use wacore_binary::Jid;
use waproto::whatsapp as wa;

use super::class::{self, Class, Content, Modifier};
use super::{Ctx, GroupFacts, context, person_of};
use crate::media;

/// Who sent the modifier and when: what a key is read against.
pub(super) struct Who<'a> {
    /// The conversation of the row (a target's conversation is always this one).
    pub(super) conv: &'a ConvKey,
    /// The person who sent the modifier (as the stanza names him).
    pub(super) sender: &'a UserId,
    /// The row is one of OUR other devices'.
    pub(super) from_me: bool,
    /// The server's time of the stanza, ms.
    pub(super) at: i64,
    /// The stanza's own id (an edit's `new_local`).
    pub(super) id: &'a str,
    pub(super) ctx: &'a Ctx,
    /// The store's facts about a group's revoke (W6a2); `None`: not read yet.
    pub(super) facts: Option<&'a GroupFacts>,
}

impl Who<'_> {
    /// The modifier's author for an event: `None` is our own account.
    pub(super) fn by(&self) -> Option<UserId> {
        (!self.from_me).then(|| self.sender.clone())
    }

    /// Whether the key names the SENDER's own message: `fromMe`, or (in a group, where the key names its
    /// author) a `participant` that is the sender himself as the stanza names him.
    fn senders_own(&self, key: &wa::MessageKey) -> bool {
        if key.from_me == Some(true) {
            return true;
        }
        key.participant
            .as_deref()
            .filter(|p| !p.is_empty())
            .and_then(|p| p.parse::<Jid>().ok())
            .and_then(|jid| person_of(&jid))
            .is_some_and(|named| &named == self.sender)
    }

    /// The sender's OWN message that a key names whatever form it writes the sender in: the sender (as the stanza names
    /// him), or `Own` from our own phone. `None` when the key states no valid id.
    fn own_target(&self, key: &wa::MessageKey) -> Option<MsgTarget> {
        let local = key.id.as_deref().filter(|id| validate_local(id).is_ok())?;
        Some(MsgTarget {
            conv: Some(self.conv.clone()),
            author: if self.from_me {
                TargetAuthor::Own
            } else {
                TargetAuthor::User(self.sender.clone())
            },
            local: local.into(),
            alternates: Vec::new(),
        })
    }

    /// The message a key names. `None` when the key states no valid id.
    fn target(&self, key: &wa::MessageKey) -> Option<MsgTarget> {
        let local = key.id.as_deref().filter(|id| validate_local(id).is_ok())?;
        let author = if key.from_me == Some(true) {
            // The target is the sender's own message.
            if self.from_me {
                TargetAuthor::Own
            } else {
                TargetAuthor::User(self.sender.clone())
            }
        } else {
            match self.conv {
                // `remoteJid` is the chat as the sender sees it: the other one of the two.
                ConvKey::Direct(peer) if self.from_me => TargetAuthor::User(peer.clone()),
                ConvKey::Direct(_) => TargetAuthor::Own,
                _ => context::quoted_author(key.participant.as_deref(), self.ctx).0,
            }
        };
        Some(MsgTarget {
            conv: Some(self.conv.clone()),
            author,
            local: local.into(),
            alternates: Vec::new(),
        })
    }
}

/// What a reaction, an edit or a revoke came to.
pub(super) enum Modified {
    /// The event on the target.
    Events(Vec<BackendEvent>),
    /// The item names no target this build can use (no key, an id that is not one, an edit of something that is not
    /// text or a caption): the row is kept.
    Keep,
    /// A group's revoke the store's facts must judge, and none were given (W6a2).
    NeedsFacts,
    /// An admin's delete-for-everyone of this group, whose member list the core was not told yet: the row waits
    /// (W6a2, D11).
    WaitsForMembers(ConvKey),
}

/// The events of a reaction, an edit or a revoke.
pub(super) fn events(kind: Modifier, base: &wa::Message, who: &Who<'_>) -> Modified {
    let event = match kind {
        Modifier::Reaction => reaction(base, who),
        Modifier::Edit => edit(base, who),
        Modifier::Revoke => return revoke(base, who),
        // The timer setting is a notice with the chat's upsert: `timer_setting`.
        Modifier::TimerSetting => None,
    };
    event.map_or(Modified::Keep, |event| Modified::Events(vec![event]))
}

/// `reactionMessage{key, text}`: an empty text takes the sender's reaction back.
fn reaction(base: &wa::Message, who: &Who<'_>) -> Option<BackendEvent> {
    let m = base.reaction_message.as_option()?;
    let target = who.target(m.key.as_option()?)?;
    let emoji = m.text.clone().unwrap_or_default();
    Some(BackendEvent::Reaction {
        target,
        by: who.by(),
        remove: emoji.is_empty(),
        emoji,
        at: who.at,
    })
}

/// A protocol message of type `MESSAGE_EDIT`: the new text of a text, or the new caption of a file.
fn edit(base: &wa::Message, who: &Who<'_>) -> Option<BackendEvent> {
    let p = base.protocol_message.as_option()?;
    let target = who.target(p.key.as_option()?)?;
    // The edited message is a plain `Message` inside the protocol message (the pinned upstream does not
    // peel it): its wrappers go through the table like any message's.
    let edited = class::peel(p.edited_message.as_option()?);
    let info = context::info_of(edited);
    let body = match class::class_of(edited) {
        Class::Text => {
            let text = wacore::proto_helpers::MessageExt::text_content(edited)
                .filter(|t| !t.is_empty())?;
            Body::Text(context::marked(text, info, who.ctx).text)
        }
        // A file takes only its caption (an edit never changes a file); no caption removes it.
        Class::Content(
            content @ (Content::Image | Content::Video | Content::VideoNote | Content::Document),
        ) => {
            let file = media::file_of(edited, content)?;
            Body::Media {
                attachments: Vec::new(),
                caption: file
                    .caption
                    .map(|c| context::marked(&c, info, who.ctx).text),
            }
        }
        _ => return None,
    };
    Some(BackendEvent::Edited {
        target,
        by: who.by(),
        new_local: Some(who.id.into()),
        body,
        edited_at: who.at,
        keeps_quote: None,
    })
}

/// A protocol message of type `REVOKE`: delete for everyone.
///
/// IN A 1:1 CHAT it is the message's own author's, or our own phone's (nobody else can delete there).
///
/// IN A GROUP (W6a2, D11, M2, M3) a revoke is one of three things, told by its key:
/// 1. the SENDER'S OWN message: `fromMe`, or a `participant` that is the sender himself as the stanza names him;
/// 2. the sender's own message again when the `participant` is the sender's OTHER id by a pair the store holds
///    (`lid_pn_mapping`: a member whose message was keyed by his number while he sends by his LID), or, from our own
///    phone, one of our two ids. The delete is then the author's, stated with the sender as author AND deleter (the
///    core takes any other `by` for an admin's and drops it unless that admin is on its list);
/// 3. ANYBODY ELSE'S message: an admin's delete (WhatsApp lets only admins delete others' messages). The core applies it
///    only when the conversation states `admin_delete` AND its member list names the deleter as an admin, and drops it
///    FOR GOOD otherwise (it is finished at once, the message would stay): so it is mapped only when the core was told
///    the group's whole member list ([`GroupFacts::members_stated`]), and until then it WAITS
///    ([`Modified::WaitsForMembers`]; stage 2 parks the row and the zone that states the members lets it go).
///
/// Without the facts the map can only tell the first from the rest ([`Modified::NeedsFacts`]).
fn revoke(base: &wa::Message, who: &Who<'_>) -> Modified {
    let Some(key) = base
        .protocol_message
        .as_option()
        .and_then(|p| p.key.as_option())
    else {
        return Modified::Keep;
    };
    let event = |target: Option<MsgTarget>, by: Option<UserId>| {
        target.map_or(Modified::Keep, |target| {
            Modified::Events(vec![BackendEvent::Deleted {
                target,
                by,
                for_everyone: true,
                at: who.at,
            }])
        })
    };
    if !matches!(who.conv, ConvKey::Group(_)) || who.senders_own(key) {
        return event(who.target(key), who.by());
    }
    let Some(facts) = who.facts else {
        return Modified::NeedsFacts;
    };
    if let Some(participant) = key
        .participant
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(|p| p.parse::<Jid>().ok())
        .and_then(|jid| person_of(&jid))
        && (facts.one_person(who.sender, &participant)
            || (who.from_me && who.ctx.own.is_own(&participant)))
    {
        return event(who.own_target(key), who.by());
    }
    if !facts.members_stated(who.conv) {
        return Modified::WaitsForMembers(who.conv.clone());
    }
    event(who.target(key), who.by())
}

/// A protocol message of type `EPHEMERAL_SETTING`: the chat's timer (0 is off), with the version that lets an
/// old setting that arrives late lose, and the timer the notice names. It sets the CHAT's timer only: it
/// gives no existing or later message a timer (F1).
pub(super) fn timer_setting(
    base: &wa::Message,
    conv: &ConvKey,
) -> Option<(ConversationPatch, Option<Duration>)> {
    let p = base.protocol_message.as_option()?;
    let timer = p
        .ephemeral_expiration
        .filter(|seconds| *seconds > 0)
        .map(|seconds| Duration::from_secs(u64::from(seconds)));
    let mut patch = ConversationPatch::new(conv.clone());
    patch.timer = timer.map_or(Patch::Clear, Patch::Set);
    patch.timer_version = p
        .ephemeral_setting_timestamp
        .and_then(|seconds| u32::try_from(seconds).ok())
        .filter(|version| *version > 0);
    Some((patch, timer))
}

/// The notice of a timer change.
pub(super) fn timer_notice(who: &Who<'_>, timer: Option<Duration>) -> Body {
    Body::System(SystemEvent::TimerChanged {
        by: who.by(),
        timer,
    })
}
