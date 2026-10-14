//! What the user may do with one message (§3.8, §4.8 step 7): the rules behind
//! [`MessageActions`](crate::facade::MessageActions) on a timeline row, and the same rules the
//! commands check before they change anything ([`crate::modify`] for edits, deletes and
//! reactions, [`crate::outbox`] for a reply). A flag on a row is the answer of the function the
//! command calls, on the same facts, so the UI never re-implements a window or a limit and a
//! shown action is refused only when the facts changed in between (the window ended, the
//! message was deleted, its send started).
//!
//! The rules look at the message and its conversation only. What depends on the command's own
//! input (an empty text, an emoji, a text above the size limit, a caption a file kind does not
//! take) is checked by the command after these.

use kourier_model::{ConversationFeatures, MessageId, Window};

use crate::attachments::policy;
use crate::error::{CoreError, Refusal, Result};
use crate::facade::MessageActions;

/// Where the message's own send stands, for the delete rules (§4.8 step 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sending {
    /// Not an own message that still waits in the outbox with nobody having it.
    Settled,
    /// Its transmit is in flight: it cannot be deleted until the answer is there.
    InFlight,
    /// An own message that no recipient is known to have: a delete takes it back.
    /// `attempted`: an attempt may have reached the server all the same. It waits for a retry
    /// after an attempt without an answer or with a temporary failure, or its send gave up
    /// when the retry budget ran out on such attempts (a time-out says nothing about what the
    /// server did with the request). "Retry" does not reset it: the row keeps one attempt on
    /// record ([`crate::outbox::retry`]) until the retried send is answered.
    Unsent {
        /// An earlier attempt got no answer or a temporary failure.
        attempted: bool,
    },
}

/// The attachments of a `Media` message, for the edit rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Media {
    /// Every file kind of the message may carry a caption in this chat.
    pub captionable: bool,
    /// The message has a caption (it can be removed even where none can be added).
    pub has_caption: bool,
    /// How many files it has (a long text beside them is not one).
    pub files: usize,
}

/// The facts the rules look at.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Facts<'a> {
    /// The account is linked (`AccountState::Active`).
    pub active: bool,
    /// The conversation is blocked.
    pub blocked: bool,
    /// The conversation's reported features; `None` until the backend sent them (then only
    /// the rules that need no feature apply, and the backend decides).
    pub features: Option<&'a ConversationFeatures>,
    /// One of our ids is an admin of the conversation.
    pub admin: bool,
    /// Our account wrote the message.
    pub own: bool,
    /// `messages.body_kind`.
    pub body_kind: &'a str,
    /// `messages.deleted` (`ForEveryone` or `ForMe`).
    pub deleted: Option<&'a str>,
    /// Sender's timestamp.
    pub sent_at: i64,
    /// Edits so far.
    pub revision: u16,
    /// Set for a `Media` body.
    pub media: Option<Media>,
    /// The message has text to copy.
    pub has_text: bool,
    /// Its own send.
    pub sending: Sending,
    /// The chat with ourselves (Signal's Note to Self): edits have no window and no count.
    pub note_to_self: bool,
    /// The message's own send row waits in the outbox or is in flight ([`send_waits`]): its
    /// request names the message's files and will be sent without anyone asking. Not a
    /// `Failed` row that is only kept for "Retry".
    pub queued: bool,
}

impl Facts<'_> {
    /// Whether an edit of the message changes the message itself instead of adding a revision
    /// (§4.8 step 5): an own message that was never handed to the network (or that the network
    /// refused for good, kept for "Retry") and has no edit waiting. Nobody has an earlier
    /// text, and nothing of the edit is sent: the message goes out with the new one.
    pub(crate) fn edits_in_place(&self) -> bool {
        self.own && self.revision == 0 && self.sending == (Sending::Unsent { attempted: false })
    }
}

/// How a delete is carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delete {
    /// Nobody has the message: it is removed with its outbox row (§4.8 step 5).
    TakeBack,
    /// The row becomes a tombstone.
    Tombstone,
}

/// Whole seconds from `from_ms` to `to_ms` (0 when negative).
pub(crate) fn age_secs(from_ms: i64, to_ms: i64) -> u64 {
    u64::try_from(to_ms.saturating_sub(from_ms).max(0) / 1000).unwrap_or(0)
}

/// The first instant (ms) at which a message sent at `sent_at` is too old for `w`: ages count
/// in whole seconds and `max_age_secs` itself is still allowed.
fn window_end(w: &Window, sent_at: i64) -> Option<i64> {
    w.max_age_secs
        .map(|max| sent_at.saturating_add((i64::from(max).saturating_add(1)).saturating_mul(1000)))
}

/// Sending anything needs a linked account and an unblocked chat that lets us send.
pub(crate) fn may_send(f: &Facts<'_>) -> Result<()> {
    if !f.active {
        return Err(CoreError::InvalidState("the account is not linked"));
    }
    if f.blocked {
        return Err(CoreError::refused(
            Refusal::Blocked,
            "the conversation is blocked",
        ));
    }
    if f.features.is_some_and(|x| x.cannot_send.is_some()) {
        return Err(CoreError::refused(
            Refusal::CannotSend,
            "you cannot send in this chat",
        ));
    }
    Ok(())
}

/// Nothing but a delete for me applies to a tombstone.
fn not_deleted(f: &Facts<'_>) -> Result<()> {
    match f.deleted {
        Some(_) => Err(CoreError::refused(
            Refusal::Deleted,
            "the message is deleted",
        )),
        None => Ok(()),
    }
}

/// System notices and placeholders are not content of a sender.
fn is_content(f: &Facts<'_>) -> bool {
    !matches!(f.body_kind, "System" | "Placeholder")
}

/// Whether the text (or the caption) of the message may be edited now; `Ok` carries the
/// instant the edit window ends (`None`: no time limit).
pub(crate) fn edit(f: &Facts<'_>, now: i64) -> Result<Option<i64>> {
    not_deleted(f)?;
    may_send(f)?;
    if !f.own {
        return Err(CoreError::refused(
            Refusal::NotOwn,
            "only your own messages can be edited",
        ));
    }
    match (f.body_kind, f.media) {
        ("Text", _) => {}
        ("Media", Some(m)) if m.captionable || m.has_caption => {}
        ("Media", Some(_)) => {
            return Err(CoreError::refused(
                Refusal::NotAllowedHere,
                "this chat takes no caption with this kind of file",
            ));
        }
        _ => {
            return Err(CoreError::refused(
                Refusal::WrongKind,
                "only texts and captions can be edited",
            ));
        }
    }
    // An edit in place is not an edit on the network: whether the chat allows editing, how
    // long and how often are rules for edits that are sent, and none is. Signal Desktop offers
    // no edit for an unsent message at all (`SD/ts/util/canEditMessage.dom.ts` needs a sent
    // state); that is its own choice, no server or client sees the difference.
    if f.edits_in_place() {
        return Ok(None);
    }
    let Some(features) = f.features else {
        return Ok(None);
    };
    // Signal: ≤ 24 h and ≤ 10 edits when sending; neither limit in Note to Self
    // (`SD/ts/util/canEditMessage.dom.ts`: `isMoreRecentThan(sent_at, DAY) ||
    // isMessageNoteToSelf`, and `isWithinMaxEdits`).
    let w = features.edit.ok_or(CoreError::refused(
        Refusal::NotAllowedHere,
        "this chat does not allow editing",
    ))?;
    if f.note_to_self {
        return Ok(None);
    }
    if w.max_age_secs
        .is_some_and(|max| age_secs(f.sent_at, now) > u64::from(max))
    {
        return Err(CoreError::refused(
            Refusal::TooOld,
            "the message is too old to edit",
        ));
    }
    if w.max_count.is_some_and(|max| f.revision >= max) {
        return Err(CoreError::refused(
            Refusal::EditedTooOften,
            "the message was edited too often",
        ));
    }
    Ok(window_end(&w, f.sent_at))
}

/// Whether the message may be deleted now, for everyone or for this account, and how; `Ok`
/// also carries the instant the delete window ends (`None`: no time limit).
pub(crate) fn delete(f: &Facts<'_>, for_everyone: bool, now: i64) -> Result<(Delete, Option<i64>)> {
    // A tombstone of a delete for everyone can still be deleted for me (that hides it; a
    // delete for me wins over one for everyone, as in ingest); nothing else applies to it.
    if f.deleted.is_some() && !(f.deleted == Some("ForEveryone") && !for_everyone) {
        return Err(CoreError::refused(
            Refusal::Deleted,
            "the message is deleted",
        ));
    }
    match f.sending {
        Sending::InFlight => {
            return Err(CoreError::refused(
                Refusal::BeingSent,
                "the message is being sent",
            ));
        }
        // A message waiting for a retry was attempted before, and so was one whose send gave
        // up on temporary failures; an attempt without an answer (a dropped reply, a crash, a
        // time-out) or with a temporary failure may have reached the server: a delete for
        // everyone then goes out as well, in case somebody has it.
        Sending::Unsent { attempted } if !(for_everyone && attempted) => {
            return Ok((Delete::TakeBack, None));
        }
        Sending::Unsent { .. } | Sending::Settled => {}
    }
    if !for_everyone {
        return Ok((Delete::Tombstone, None));
    }
    may_send(f)?;
    if !is_content(f) {
        return Err(CoreError::refused(
            Refusal::WrongKind,
            "this message cannot be deleted for everyone",
        ));
    }
    if !f.own && !(f.admin && f.features.is_none_or(|x| x.admin_delete)) {
        return Err(CoreError::refused(
            Refusal::NotOwn,
            "only your own messages can be deleted for everyone",
        ));
    }
    let Some(features) = f.features else {
        return Ok((Delete::Tombstone, None));
    };
    // Signal: `global.normalDeleteMaxAgeInSeconds`, one day by default.
    let w = features.delete_for_everyone.ok_or(CoreError::refused(
        Refusal::NotAllowedHere,
        "this chat does not allow deleting for everyone",
    ))?;
    if !w.allows(age_secs(f.sent_at, now), 0) {
        return Err(CoreError::refused(
            Refusal::TooOld,
            "the message is too old to delete for everyone",
        ));
    }
    Ok((Delete::Tombstone, window_end(&w, f.sent_at)))
}

/// Whether we may react to the message; `Ok` carries the chat's reactions per user (`None`:
/// an unlimited set).
pub(crate) fn react(f: &Facts<'_>) -> Result<Option<u8>> {
    not_deleted(f)?;
    may_send(f)?;
    if !is_content(f) {
        return Err(CoreError::refused(
            Refusal::WrongKind,
            "this message takes no reactions",
        ));
    }
    match f.features {
        Some(x) if !x.reactions.is_usable() => Err(CoreError::refused(
            Refusal::NotAllowedHere,
            "this chat has no reactions",
        )),
        Some(x) => Ok(x.reactions_per_user),
        // Not reported yet: Signal and WhatsApp semantics (one reaction per user).
        None => Ok(Some(1)),
    }
}

/// Whether a new message may quote this one.
pub(crate) fn reply(f: &Facts<'_>) -> Result<()> {
    if f.deleted.is_some() {
        return Err(CoreError::refused(
            Refusal::Deleted,
            "the quoted message is deleted",
        ));
    }
    may_send(f)?;
    if !is_content(f) {
        return Err(CoreError::refused(
            Refusal::WrongKind,
            "this message cannot be quoted",
        ));
    }
    if f.features.is_some_and(|x| !x.reply.is_usable()) {
        return Err(CoreError::refused(
            Refusal::NotAllowedHere,
            "this chat has no replies",
        ));
    }
    Ok(())
}

/// Whether one attachment of the message may be deleted for this account now
/// (`UiCommand::DeleteAttachment`). It needs no linked account and no window: the delete is
/// local, and our other devices are told when the account can. A message whose own send is
/// still on its way keeps its files, because that send names them: one that waits or is in
/// flight ([`Facts::queued`]), and one that nobody has yet (it can be cancelled or deleted
/// whole). A message that reached some recipients and failed for the others takes the delete
/// like any sent message, as it takes the same delete made on the phone
/// (`AttachmentDeleted`): its row only waits for "Retry", and a retry sends the message as it
/// is then, without the file ([`crate::outbox`] `prune_gone_files`).
pub(crate) fn delete_attachment(f: &Facts<'_>) -> Result<()> {
    not_deleted(f)?;
    match (f.body_kind, f.media) {
        ("Media", Some(m)) if m.files > 0 => {}
        ("Sticker", _) => {}
        _ => {
            return Err(CoreError::refused(
                Refusal::WrongKind,
                "this message has no attachment",
            ));
        }
    }
    if f.queued || f.sending != Sending::Settled {
        return Err(CoreError::refused(
            Refusal::BeingSent,
            "the message is not sent yet",
        ));
    }
    Ok(())
}

/// Whether the message has text the UI can copy.
pub(crate) fn copy(f: &Facts<'_>) -> bool {
    f.deleted.is_none() && f.has_text
}

/// The attachments of a message whose Download asks the other device
/// ([`MessageActions::ask_attachments`]): `can` is the message's part of `Apply::can_ask`
/// (account linked, network can ask, message named, not view-once, a chat and an author with
/// a stable identity); the rest is [`policy::may_click`] on each file of `body` exactly as the
/// click checks it. One being asked for already is not offered: its click would only join the
/// open request.
pub(crate) fn ask_attachments(
    can: bool,
    deleted: Option<&str>,
    body: &kourier_model::Body,
    receive_limit: Option<u64>,
) -> Vec<kourier_model::AttachmentId> {
    use kourier_model::{AttachmentState, Body};
    if !can || deleted.is_some() {
        return Vec::new();
    }
    let files: Vec<&kourier_model::Attachment> = match body {
        Body::Media { attachments, .. } => attachments.iter().collect(),
        Body::Sticker(a) => vec![a.as_ref()],
        _ => return Vec::new(),
    };
    files
        .into_iter()
        .filter(|a| {
            !a.long_text
                && a.state != AttachmentState::Asking
                && policy::may_click(a.state, a.remote.is_some(), a.size, receive_limit, true)
                    == Ok(policy::Click::Ask)
        })
        .filter_map(|a| a.id)
        .collect()
}

/// Every flag of one message at `now` (the core clock).
pub(crate) fn all(f: &Facts<'_>, now: i64) -> MessageActions {
    let edit = edit(f, now);
    let for_everyone = delete(f, true, now);
    let mut until: Option<i64> = None;
    let mut ends = |at: Option<i64>| {
        if let Some(at) = at {
            until = Some(until.map_or(at, |u| u.min(at)));
        }
    };
    if let Ok(at) = &edit {
        ends(*at);
    }
    if let Ok((_, at)) = &for_everyone {
        ends(*at);
    }
    MessageActions {
        reply: reply(f).is_ok(),
        react: react(f).is_ok(),
        edit: edit.is_ok(),
        delete_for_everyone: for_everyone.is_ok(),
        delete_for_me: delete(f, false, now).is_ok(),
        delete_attachment: delete_attachment(f).is_ok(),
        copy: copy(f),
        take_back: matches!(for_everyone, Ok((Delete::TakeBack, _))),
        // Needs the attachments' rows: the timeline query sets it ([`ask_attachments`]).
        ask_attachments: Vec::new(),
        until,
    }
}

/// The send facts of own messages of one conversation that still have their `Message` row in
/// the outbox: message → [`Sending`]. Messages without an entry are [`Sending::Settled`].
pub(crate) fn sending_of(
    conn: &kourier_db::rusqlite::Connection,
    message: MessageId,
) -> Result<Sending> {
    use kourier_db::rusqlite::OptionalExtension;
    let row: Option<(String, i64)> = conn
        .prepare_cached(
            "SELECT state, attempts FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
        )?
        .query_row([message.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((state, attempts)) = row else {
        return Ok(Sending::Settled);
    };
    if state == crate::outbox::TRANSMITTING {
        return Ok(Sending::InFlight);
    }
    let reached = crate::outbox::send_states(conn, message)?
        .values()
        .any(|s| !s.needs_send());
    if reached {
        return Ok(Sending::Settled);
    }
    // A `Failed` row the network refused for good reached nobody. One that failed because its
    // budget ran out (`retryable`) ended on attempts that got no definite answer.
    let gave_up = || -> Result<bool> {
        Ok(matches!(
            crate::outbox::floor(conn, message)?,
            Some(kourier_model::OutgoingState::Failed {
                retryable: true,
                ..
            })
        ))
    };
    let attempted = attempts > 0 && (state == crate::outbox::QUEUED || gave_up()?);
    Ok(Sending::Unsent { attempted })
}

/// Whether the own send of `message` waits in the outbox or is in flight ([`Facts::queued`]):
/// its `Message` row is queued, being stored as part of a large send, or transmitting. A
/// `Failed` row does not count: nothing sends it unless the user asks.
pub(crate) fn send_waits(
    conn: &kourier_db::rusqlite::Connection,
    message: MessageId,
) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM outbox WHERE message_id = ?1 AND kind = 'Message'
               AND state <> 'Failed')",
        )?
        .query_row([message.0], |r| r.get(0))?)
}

/// The [`Media`] facts of a `Media` body in a chat with `features`: long-text attachments
/// (the rest of the text, not a file) do not count.
pub(crate) fn media_of(
    body: &kourier_model::Body,
    features: Option<&ConversationFeatures>,
) -> Option<Media> {
    let kourier_model::Body::Media {
        attachments,
        caption,
    } = body
    else {
        return None;
    };
    let captionable = attachments.iter().filter(|a| !a.long_text).all(|a| {
        features.is_none_or(|f| f.files.get(&a.kind).is_some_and(|limits| limits.caption))
    });
    Some(Media {
        captionable,
        has_caption: caption.as_ref().is_some_and(|c| !c.body.trim().is_empty()),
        files: attachments.iter().filter(|a| !a.long_text).count(),
    })
}

/// Whether a body has text to copy.
pub(crate) fn has_text(body: &kourier_model::Body) -> bool {
    use kourier_model::Body;
    let filled = |t: &kourier_model::Text| !t.body.trim().is_empty();
    match body {
        Body::Text(t) => filled(t),
        Body::Media { caption, .. } => caption.as_ref().is_some_and(filled),
        Body::Sticker(a) => a.caption.as_ref().is_some_and(filled),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::Support;

    const DAY: i64 = 24 * 3600 * 1000;

    fn features() -> ConversationFeatures {
        ConversationFeatures {
            reply: Support::Full,
            reactions: Support::Full,
            reactions_per_user: Some(1),
            edit: Some(Window {
                max_age_secs: Some(24 * 3600),
                max_count: Some(10),
            }),
            delete_for_everyone: Some(Window {
                max_age_secs: Some(3 * 3600),
                max_count: None,
            }),
            delete_for_me: true,
            admin_delete: true,
            ..ConversationFeatures::default()
        }
    }

    fn own<'a>(features: &'a ConversationFeatures) -> Facts<'a> {
        Facts {
            active: true,
            blocked: false,
            features: Some(features),
            admin: false,
            own: true,
            body_kind: "Text",
            deleted: None,
            sent_at: 1_000_000,
            revision: 0,
            media: None,
            has_text: true,
            sending: Sending::Settled,
            note_to_self: false,
            queued: false,
        }
    }

    /// Signal Desktop lets Note to Self edit without the window and the count; the other
    /// rules (own, text, a chat that allows editing) stay.
    #[test]
    fn note_to_self_edits_have_no_window_and_no_count() {
        let f = features();
        let old = Facts {
            revision: 40,
            ..own(&f)
        };
        let later = old.sent_at + 400 * DAY;
        assert!(
            edit(&old, later).is_err(),
            "an ordinary chat has both limits"
        );
        let mine = Facts {
            note_to_self: true,
            ..old
        };
        assert_eq!(edit(&mine, later).ok(), Some(None), "no end either");
        let a = all(&mine, later);
        assert!(
            a.edit && !a.delete_for_everyone,
            "only the edit rule differs"
        );
        assert_eq!(a.until, None);
        let no_edits = ConversationFeatures {
            edit: None,
            ..features()
        };
        let refused = Facts {
            features: Some(&no_edits),
            ..mine
        };
        assert!(edit(&refused, later).is_err());
        let theirs = Facts { own: false, ..mine };
        assert!(edit(&theirs, later).is_err());
    }

    #[test]
    fn windows_end_one_second_after_the_last_allowed_age() {
        let f = features();
        let facts = own(&f);
        let a = all(&facts, facts.sent_at);
        assert!(a.reply && a.react && a.edit && a.delete_for_everyone && a.delete_for_me && a.copy);
        assert!(!a.take_back);
        // The delete window (3 h) ends before the edit window (24 h).
        let delete_end = facts.sent_at + (3 * 3600 + 1) * 1000;
        assert_eq!(a.until, Some(delete_end));
        let before = all(&facts, delete_end - 1);
        assert!(before.delete_for_everyone && before.edit);
        let after = all(&facts, delete_end);
        assert!(!after.delete_for_everyone && after.edit);
        assert_eq!(after.until, Some(facts.sent_at + DAY + 1000));
        let late = all(&facts, facts.sent_at + DAY + 1000);
        assert!(!late.edit && !late.delete_for_everyone && late.delete_for_me);
        assert_eq!(late.until, None);
        // The flag and the command's check are one function.
        assert!(edit(&facts, delete_end).is_ok());
        assert!(delete(&facts, true, delete_end).is_err());
    }

    #[test]
    fn others_messages_tombstones_and_notices() {
        let f = features();
        let theirs = Facts {
            own: false,
            ..own(&f)
        };
        let a = all(&theirs, theirs.sent_at);
        assert!(a.reply && a.react && a.copy && a.delete_for_me);
        assert!(!a.edit && !a.delete_for_everyone);
        assert_eq!(a.until, None);
        let as_admin = all(
            &Facts {
                admin: true,
                ..theirs
            },
            theirs.sent_at,
        );
        assert!(as_admin.delete_for_everyone && !as_admin.edit);

        let tombstone = Facts {
            deleted: Some("ForEveryone"),
            ..own(&f)
        };
        let a = all(&tombstone, tombstone.sent_at);
        assert!(a.delete_for_me, "hiding the tombstone");
        assert!(!a.reply && !a.react && !a.edit && !a.delete_for_everyone && !a.copy);

        let notice = Facts {
            body_kind: "System",
            has_text: false,
            ..own(&f)
        };
        let a = all(&notice, notice.sent_at);
        assert!(a.delete_for_me);
        assert!(!a.reply && !a.react && !a.edit && !a.delete_for_everyone && !a.copy);
    }

    #[test]
    fn chats_that_cannot_send_and_unknown_features() {
        let f = features();
        for facts in [
            Facts {
                active: false,
                ..own(&f)
            },
            Facts {
                blocked: true,
                ..own(&f)
            },
        ] {
            let a = all(&facts, facts.sent_at);
            assert!(!a.reply && !a.react && !a.edit && !a.delete_for_everyone);
            assert!(a.delete_for_me && a.copy);
        }
        let none = Facts {
            features: None,
            ..own(&f)
        };
        let a = all(&none, none.sent_at + 100 * DAY);
        assert!(
            a.reply && a.react && a.edit && a.delete_for_everyone,
            "the backend decides"
        );
        assert_eq!(a.until, None);
        let no_reply = ConversationFeatures {
            reply: Support::Unsupported,
            reactions: Support::Unsupported,
            edit: None,
            delete_for_everyone: None,
            ..features()
        };
        let a = all(&own(&no_reply), 1_000_000);
        assert!(!a.reply && !a.react && !a.edit && !a.delete_for_everyone && a.delete_for_me);
    }

    #[test]
    fn unsent_messages_are_taken_back() {
        let f = features();
        let queued = Facts {
            sending: Sending::Unsent { attempted: false },
            sent_at: 0,
            ..own(&f)
        };
        // Far outside the delete window: nobody has it, so it is simply taken back.
        let a = all(&queued, 100 * DAY);
        assert!(a.delete_for_everyone && a.delete_for_me && a.take_back);
        let retrying = Facts {
            sending: Sending::Unsent { attempted: true },
            ..queued
        };
        assert_eq!(
            delete(&retrying, false, 0).expect("for me").0,
            Delete::TakeBack
        );
        assert_eq!(
            delete(&retrying, true, 0).expect("for everyone").0,
            Delete::Tombstone,
            "an attempt may have reached the server"
        );
        let in_flight = Facts {
            sending: Sending::InFlight,
            ..queued
        };
        let a = all(&in_flight, 0);
        assert!(!a.delete_for_everyone && !a.delete_for_me);
    }

    /// An edit of a message nobody has changes the message itself and sends nothing, so
    /// whatever the chat says about edits (none at all, a window, a count) does not bind it.
    /// As soon as an attempt may have reached the server the edit is one for the network, and
    /// all of it applies again.
    #[test]
    fn an_edit_in_place_is_not_bound_by_the_chats_edit_rules() {
        let no_edits = ConversationFeatures {
            edit: None,
            ..features()
        };
        let queued = Facts {
            sending: Sending::Unsent { attempted: false },
            sent_at: 0,
            features: Some(&no_edits),
            ..own(&no_edits)
        };
        assert!(queued.edits_in_place());
        assert_eq!(
            edit(&queued, 400 * DAY).ok(),
            Some(None),
            "no chat rule, no end"
        );
        let a = all(&queued, 400 * DAY);
        assert!(a.edit && a.take_back);
        assert_eq!(a.until, None);

        let f = features();
        let attempted = Facts {
            sending: Sending::Unsent { attempted: true },
            sent_at: 0,
            ..own(&f)
        };
        assert!(!attempted.edits_in_place());
        assert!(edit(&attempted, 400 * DAY).is_err(), "the window is back");
        assert!(edit(&attempted, 0).is_ok());
        assert!(
            edit(
                &Facts {
                    features: Some(&no_edits),
                    ..attempted
                },
                0
            )
            .is_err(),
            "and so is the chat's say on edits"
        );
        // An edit that already waits is a revision; the next one is a revision behind it.
        let edited = Facts {
            revision: 1,
            ..queued
        };
        assert!(!edited.edits_in_place());
        assert!(edit(&edited, 0).is_err(), "this chat has no edits to send");
        // What the message itself must satisfy still holds: ours, not deleted, a chat we can
        // send in, a text or a caption the files take.
        assert!(
            edit(
                &Facts {
                    own: false,
                    ..queued
                },
                0
            )
            .is_err()
        );
        assert!(
            edit(
                &Facts {
                    deleted: Some("ForMe"),
                    ..queued
                },
                0
            )
            .is_err()
        );
        assert!(
            edit(
                &Facts {
                    blocked: true,
                    ..queued
                },
                0
            )
            .is_err()
        );
        assert!(
            edit(
                &Facts {
                    body_kind: "Sticker",
                    ..queued
                },
                0
            )
            .is_err()
        );
    }

    #[test]
    fn captions_are_edited_where_the_files_take_one() {
        let f = features();
        let media = |captionable, has_caption| Facts {
            body_kind: "Media",
            media: Some(Media {
                captionable,
                has_caption,
                files: 1,
            }),
            ..own(&f)
        };
        assert!(edit(&media(true, false), 1_000_000).is_ok());
        assert!(
            edit(&media(false, true), 1_000_000).is_ok(),
            "it can be removed"
        );
        assert!(edit(&media(false, false), 1_000_000).is_err());
        let sticker = Facts {
            body_kind: "Sticker",
            ..own(&f)
        };
        assert!(edit(&sticker, 1_000_000).is_err());
        let too_often = Facts {
            revision: 10,
            ..own(&f)
        };
        assert!(edit(&too_often, 1_000_000).is_err());
    }

    /// Each refusal says why in a form a UI can tell apart, and keeps its sentence for the log.
    #[test]
    fn refusals_carry_a_reason_a_ui_can_map() {
        let f = features();
        let reason = |r: Result<Option<i64>>| r.expect_err("refused").refusal();
        let day = 24 * 3600 * 1000;
        let old = edit(&own(&f), 1_000_000 + 2 * day).expect_err("too old");
        assert_eq!(old.refusal(), Some(Refusal::TooOld));
        assert_eq!(
            old.to_string(),
            "invalid request: the message is too old to edit"
        );
        let often = Facts {
            revision: 10,
            ..own(&f)
        };
        assert_eq!(
            reason(edit(&often, 1_000_000)),
            Some(Refusal::EditedTooOften)
        );
        let tombstone = Facts {
            deleted: Some("ForEveryone"),
            ..own(&f)
        };
        assert_eq!(reason(edit(&tombstone, 1_000_000)), Some(Refusal::Deleted));
        assert_eq!(
            delete(&tombstone, true, 1_000_000)
                .expect_err("deleted")
                .refusal(),
            Some(Refusal::Deleted)
        );
        let flying = Facts {
            sending: Sending::InFlight,
            ..own(&f)
        };
        assert_eq!(
            delete(&flying, false, 1_000_000)
                .expect_err("in flight")
                .refusal(),
            Some(Refusal::BeingSent)
        );
        let theirs = Facts {
            own: false,
            ..own(&f)
        };
        assert_eq!(reason(edit(&theirs, 1_000_000)), Some(Refusal::NotOwn));
        let mut no_edits = f.clone();
        no_edits.edit = None;
        assert_eq!(
            reason(edit(&own(&no_edits), 1_000_000)),
            Some(Refusal::NotAllowedHere)
        );
        let notice = Facts {
            body_kind: "System",
            ..own(&f)
        };
        assert_eq!(reason(edit(&notice, 1_000_000)), Some(Refusal::WrongKind));
        let blocked = Facts {
            blocked: true,
            ..own(&f)
        };
        assert_eq!(reason(edit(&blocked, 1_000_000)), Some(Refusal::Blocked));
        assert_eq!(
            delete(&own(&f), true, 1_000_000 + 2 * day)
                .expect_err("too old")
                .refusal(),
            Some(Refusal::TooOld)
        );
        // An unlinked account is a state, not a refusal of this message.
        let unlinked = Facts {
            active: false,
            ..own(&f)
        };
        let e = edit(&unlinked, 1_000_000).expect_err("not linked");
        assert!(matches!(e, CoreError::InvalidState(_)) && e.refusal().is_none());
    }

    /// One attachment can be deleted for this account from any message that has one and is
    /// not waiting in the outbox; the row's flag is the command's check.
    #[test]
    fn an_attachment_is_deleted_only_from_a_message_that_is_sent() {
        let f = features();
        let media = |files| Facts {
            body_kind: "Media",
            media: Some(Media {
                captionable: true,
                has_caption: false,
                files,
            }),
            ..own(&f)
        };
        assert!(delete_attachment(&media(2)).is_ok());
        assert!(all(&media(2), 1_000_000).delete_attachment);
        let theirs = Facts {
            own: false,
            ..media(1)
        };
        assert!(delete_attachment(&theirs).is_ok(), "others' messages too");
        // Local: no linked account, no unblocked chat and no window is needed.
        let unlinked = Facts {
            active: false,
            blocked: true,
            sent_at: 0,
            ..media(1)
        };
        assert!(delete_attachment(&unlinked).is_ok());
        let sticker = Facts {
            body_kind: "Sticker",
            ..own(&f)
        };
        assert!(delete_attachment(&sticker).is_ok());
        let why = |f: &Facts<'_>| delete_attachment(f).expect_err("refused").refusal();
        assert_eq!(why(&own(&f)), Some(Refusal::WrongKind), "a text");
        assert_eq!(why(&media(0)), Some(Refusal::WrongKind), "only a long text");
        let queued = Facts {
            queued: true,
            ..media(1)
        };
        assert_eq!(why(&queued), Some(Refusal::BeingSent));
        assert!(!all(&queued, 1_000_000).delete_attachment);
        let unsent = Facts {
            sending: Sending::Unsent { attempted: false },
            ..media(1)
        };
        assert_eq!(why(&unsent), Some(Refusal::BeingSent));
        let tombstone = Facts {
            deleted: Some("ForMe"),
            ..media(1)
        };
        assert_eq!(why(&tombstone), Some(Refusal::Deleted));
    }

    /// The Download of a lost file asks the other device only where the click would, file by
    /// file: a linked account whose network can ask, a named message that is not deleted, and
    /// a file (not a long text) that the network lost and that is within the receive limit.
    /// A message with a file that is too large and one that is not offers the second alone;
    /// a file being asked for, or whose ask ended "no file" or "no message", is not offered.
    #[test]
    fn lost_files_ask_where_the_click_would() {
        use kourier_model::{AskFailure, Attachment, AttachmentId, AttachmentState, Body};
        let file = |id, state, size, long_text| Attachment {
            id: Some(AttachmentId(id)),
            state,
            size,
            long_text,
            ..Attachment::default()
        };
        let media = |atts: Vec<Attachment>| Body::Media {
            attachments: atts,
            caption: None,
        };
        let ids = |v: &[i64]| v.iter().map(|n| AttachmentId(*n)).collect::<Vec<_>>();
        let gone = media(vec![file(1, AttachmentState::Expired, 10, false)]);
        assert_eq!(ask_attachments(true, None, &gone, Some(100)), ids(&[1]));
        assert!(ask_attachments(false, None, &gone, Some(100)).is_empty());
        assert!(ask_attachments(true, Some("ForMe"), &gone, Some(100)).is_empty());
        assert!(
            ask_attachments(true, None, &gone, Some(5)).is_empty(),
            "above the limit"
        );
        let mixed = media(vec![
            file(1, AttachmentState::Unavailable, 500, false),
            file(2, AttachmentState::Unavailable, 50, false),
            file(3, AttachmentState::Local, 50, false),
            file(4, AttachmentState::Asking, 50, false),
        ]);
        assert_eq!(
            ask_attachments(true, None, &mixed, Some(100)),
            ids(&[2]),
            "the too-large file, the one that is here and the one being asked have no Download"
        );
        let text = media(vec![file(1, AttachmentState::Unavailable, 10, true)]);
        assert!(
            ask_attachments(true, None, &text, None).is_empty(),
            "a long text alone"
        );
        let ended = |why| media(vec![file(1, AttachmentState::AskFailed { why }, 10, false)]);
        assert!(ask_attachments(true, None, &ended(AskFailure::NoFile), None).is_empty());
        assert!(ask_attachments(true, None, &ended(AskFailure::NoMessage), None).is_empty());
        assert_eq!(
            ask_attachments(true, None, &ended(AskFailure::NoAnswer), None),
            ids(&[1]),
            "a request that was not answered can be made again"
        );
        let sticker = Body::Sticker(Box::new(file(1, AttachmentState::Unavailable, 10, false)));
        assert_eq!(ask_attachments(true, None, &sticker, None), ids(&[1]));
        assert!(ask_attachments(true, None, &Body::Text(Default::default()), None).is_empty());
    }
}
