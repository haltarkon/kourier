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

use kourier_model::{ConversationFeatures, FailReason, MessageId, OutgoingState, UserId, Window};

use crate::attachments::policy;
use crate::error::{CoreError, Refusal, Result};
use crate::facade::{MessageActions, ShownPerson};

/// Where the message's own send stands, for the delete rules (§4.8 step 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sending {
    /// Not an own message that still waits in the outbox with nobody having it.
    Settled,
    /// Its transmit is in flight: it cannot be deleted until the answer is there.
    /// `stoppable`: it may be stopped all the same ([`stoppable`]): the stop is a cancel
    /// ([`cancel_sending`]), never a delete.
    InFlight {
        /// [`stoppable`] holds.
        stoppable: bool,
    },
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
    /// The account's network has a backend in this run. An account whose network is turned off
    /// is readable only: nothing that would go to the network is offered, and a take-back of a
    /// message that reached nobody (a local delete) still is (K-A fix C2).
    pub registered: bool,
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
    if !f.registered {
        return Err(CoreError::InvalidState(
            "the account's network is turned off",
        ));
    }
    if f.blocked {
        return Err(CoreError::refused(
            Refusal::Blocked,
            "the conversation is blocked",
        ));
    }
    cannot_send(f.features)
}

/// THE rule of "the chat's features say we cannot send in it" (a group we left, announcements
/// only, a chat the phone has not accepted, ...): used by [`may_send`] (edit, delete, react),
/// by "Send Again" ([`retry`]) and by a change of the disappearing timer (the runtime), so that
/// everything that puts a message on the wire for the chat has one answer. NULL features (the
/// network has stated none) are sendable, as the flag `ConversationRow.can_send` and the gate
/// of a new send (`outbox::Target::read`, G4) say.
pub(crate) fn cannot_send(features: Option<&ConversationFeatures>) -> Result<()> {
    if features.is_some_and(|x| x.cannot_send.is_some()) {
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
        // Whatever a stop could do (`stoppable`), a DELETE of a message in flight is refused:
        // it would take the row away while the transmit goes on and reaches the recipients.
        Sending::InFlight { .. } => {
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

/// Whether the files of a message may be saved to a folder at all
/// ([`MessageActions::save_attachments`], [`UiCommand::SaveAttachment`]
/// (crate::facade::UiCommand::SaveAttachment)): the message is not a tombstone, not hidden
/// because its time has come on a clock nobody confirmed (`expiry_hidden`: gone for the user,
/// like a deleted one), and not view-once. A view-once message is never saved, in any form
/// (his "Show once, then remove"); the clause is load-bearing on WhatsApp, whose view-once
/// media keeps an attachment (Signal maps one with none). A message that disappears (a timer)
/// may be saved: the copy is the user's own act.
pub(crate) fn may_save_message(
    deleted: Option<&str>,
    view_once: bool,
    expiry_hidden: bool,
) -> Result<()> {
    if deleted.is_some() {
        return Err(CoreError::refused(
            Refusal::Deleted,
            "the message is deleted",
        ));
    }
    if expiry_hidden {
        return Err(CoreError::refused(Refusal::Gone, "the message is gone"));
    }
    if view_once {
        return Err(CoreError::refused(
            Refusal::WrongKind,
            "a view-once message is never saved",
        ));
    }
    Ok(())
}

/// Whether one file of such a message may be saved: it is on this PC (`Local`), and it is a
/// file, not the rest of a long text. A file that is not here has Download (a save never
/// fetches: his answer "Only once downloaded"); the networks that cannot download yet
/// (WhatsApp before W7, Telegram, Threema) never show a `Local` one, so nothing is offered
/// that does nothing.
pub(crate) fn may_save_file(state: kourier_model::AttachmentState, long_text: bool) -> Result<()> {
    if long_text {
        return Err(CoreError::refused(
            Refusal::WrongKind,
            "the rest of a text is not a file",
        ));
    }
    if state != kourier_model::AttachmentState::Local {
        return Err(CoreError::InvalidInput("the attachment is not downloaded"));
    }
    Ok(())
}

/// The attachments of a message that "Save As…" accepts ([`MessageActions::save_attachments`]):
/// [`may_save_message`] on the message, [`may_save_file`] on each file of `body`. THE one rule
/// for the flag and the command (which asks both functions again on the attachment's row, so a
/// message deleted while the dialog was open writes nothing). Stickers count: Desktop's and
/// Android's refusal to save them is their own UI choice, not a limit of the network.
pub(crate) fn save_attachments(
    deleted: Option<&str>,
    view_once: bool,
    expiry_hidden: bool,
    body: &kourier_model::Body,
) -> Vec<kourier_model::AttachmentId> {
    use kourier_model::Body;
    if may_save_message(deleted, view_once, expiry_hidden).is_err() {
        return Vec::new();
    }
    let files: Vec<&kourier_model::Attachment> = match body {
        Body::Media { attachments, .. } => attachments.iter().collect(),
        Body::Sticker(a) => vec![a.as_ref()],
        _ => return Vec::new(),
    };
    files
        .into_iter()
        .filter(|a| may_save_file(a.state, a.long_text).is_ok())
        .filter_map(|a| a.id)
        .collect()
}

/// Whether "Send Again" ([`UiCommand::RetryMessage`](crate::facade::UiCommand::RetryMessage))
/// is accepted for an own message whose send ended in `floor` (the message's stored send floor,
/// [`crate::outbox::floor`]). THE one rule for the flag ([`MessageActions::retry`]) and the
/// command ([`crate::outbox::retry`]): a message the network refused because its recipient has
/// not written to this account ([`FailReason::ReachOutRefused`]) is never sent again by
/// Kourier, because a second try is one more reach-out and every one counts against the
/// account (research §5.5: after a 463 stop, explain, never retry). It can be taken back. The
/// conversation's other waiting messages share that end ([`crate::outbox`]
/// `fail_waiting_of`). Everything else a message can fail with may be tried again.
///
/// A chat whose `features` say we cannot send in it (it lost the right after the message
/// failed: we left the group, the phone un-accepted the chat) takes no second try either
/// ([`Refusal::CannotSend`], [`cannot_send`]): the network would only fail it again. NULL
/// features are sendable.
pub(crate) fn retry(
    floor: Option<&OutgoingState>,
    features: Option<&ConversationFeatures>,
) -> Result<()> {
    if matches!(
        floor,
        Some(OutgoingState::Failed {
            reason: FailReason::ReachOutRefused,
            ..
        })
    ) {
        return Err(CoreError::refused(
            Refusal::ReachOutRefused,
            "the network refused the message as a first contact; sending it again would be one more",
        ));
    }
    cannot_send(features)
}

/// What the stop rule looks at: one own message's `Message` outbox row and the core's set of
/// messages whose upload is reported ([`crate::runtime::Shared::uploading`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct StopFacts {
    /// The row is `Transmitting`: its attempt is in flight.
    pub transmitting: bool,
    /// The backend reports an upload of the message that is still moving.
    pub uploading: bool,
    /// How many attempts of the row started (`outbox.attempts`, this one included).
    pub attempts: i64,
    /// Some recipient has the message (a `send_state` that needs no send).
    pub reached: bool,
    /// The stop was asked already (`outbox.cancel_state` is not 0: the X was pressed and the
    /// backend's answer is on its way, or it said `Stopping`): it is asked once.
    pub asked: bool,
}

/// Whether the message in flight may be STOPPED now: THE rule for the flag
/// ([`MessageActions::cancel_sending`]) and the command
/// ([`UiCommand::CancelMessage`](crate::facade::UiCommand::CancelMessage)), call 1 of the
/// upload-progress step. A stop takes the message back, so it is offered only for a message
/// that nobody can have:
/// - its attempt is in flight,
/// - the backend reports an upload of it ("an upload of it is reported": a backend that sends
///   no `AttachmentProgress` cannot answer an abort, so it never gets one),
/// - it is the FIRST attempt (`attempts == 1`): an earlier one may have reached the server (a
///   retry after a dropped reply, a temporary failure, a restart in the middle of an upload),
/// - no recipient has it (a `Partial` group send that uploads again for the members that
///   failed),
/// - the stop was not asked yet (the X greys out once it is pressed).
pub(crate) fn stoppable(f: StopFacts) -> bool {
    f.transmitting && f.uploading && f.attempts == 1 && !f.reached && !f.asked
}

/// Whether "Cancel Sending" ([`UiCommand::CancelMessage`](crate::facade::UiCommand::CancelMessage))
/// is offered: `take_back` (nothing was ever handed to the network: it is simply removed) or a
/// stop of the message in flight ([`stoppable`]).
pub(crate) fn cancel_sending(f: &Facts<'_>, take_back: bool) -> bool {
    take_back || matches!(f.sending, Sending::InFlight { stoppable: true })
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
    let take_back = matches!(for_everyone, Ok((Delete::TakeBack, _)));
    MessageActions {
        reply: reply(f).is_ok(),
        react: react(f).is_ok(),
        edit: edit.is_ok(),
        delete_for_everyone: for_everyone.is_ok(),
        delete_for_me: delete(f, false, now).is_ok(),
        delete_attachment: delete_attachment(f).is_ok(),
        copy: copy(f),
        take_back,
        cancel_sending: cancel_sending(f, take_back),
        // Needs the attachments' rows: the timeline query sets it ([`ask_attachments`]).
        ask_attachments: Vec::new(),
        // Needs the attachments' rows too: the timeline query sets it ([`save_attachments`]).
        save_attachments: Vec::new(),
        // Needs the outbox row and the stored floor: the timeline query sets it ([`retry`]).
        retry: false,
        // Need the body and the conversation's kind: the timeline query sets them
        // ([`show_sender`], [`show_people`]).
        show_sender: false,
        show_people: Vec::new(),
        // Needs the dispatcher's pauses: the timeline query sets it.
        safety_number: None,
        until,
    }
}

/// Whether "Show Sender's Page" is offered ([`MessageActions::show_sender`]): another person's
/// message in a group. A message of the user's own account (a call line without an author
/// counts as one: no author, nothing to show) and a message of a chat that is no group have no
/// such entry.
pub(crate) fn show_sender(own: bool, group: bool) -> bool {
    group && !own
}

/// The people a message's menu offers a page of ([`MessageActions::show_people`]): the
/// distinct mentions of its text or caption in the order of their first mention, our own ids
/// as ONE entry. A deleted message has none, and neither has a mention under a spoiler (the
/// core cannot know whether the spoiler was revealed, so it states none: what a spoiler hides
/// is not listed by a menu).
pub(crate) fn show_people(
    deleted: bool,
    body: &kourier_model::Body,
    own_ids: &[UserId],
) -> Vec<ShownPerson> {
    use kourier_model::text::validate_range;
    use kourier_model::{Body, RangeKind};
    let text = match body {
        Body::Text(t) => Some(t),
        Body::Media { caption, .. } => caption.as_ref(),
        Body::Sticker(a) => a.caption.as_ref(),
        _ => None,
    };
    let Some(text) = text.filter(|_| !deleted) else {
        return Vec::new();
    };
    let valid: Vec<&kourier_model::TextRange> = text
        .ranges
        .iter()
        .filter(|r| validate_range(&text.body, r).is_ok())
        .collect();
    let spoiled = |at: &kourier_model::TextRange| {
        valid.iter().any(|s| {
            matches!(s.kind, RangeKind::Spoiler)
                && s.start <= at.start
                && at.end().is_some_and(|e| s.end().is_some_and(|se| e <= se))
        })
    };
    // One mention per placeholder, as the text shows it (`Text::shown_mentions`, the renderer's rule
    // too): "everyone" and a person on one placeholder is "everyone", two persons the last.
    let mut out: Vec<ShownPerson> = Vec::new();
    for r in text.shown_mentions() {
        let RangeKind::Mention(user) = &r.kind else {
            continue;
        };
        if spoiled(r) {
            continue;
        }
        let own = own_ids.contains(user);
        let seen = out
            .iter()
            .any(|p| if own { p.own } else { p.user == *user });
        if !seen {
            out.push(ShownPerson {
                user: user.clone(),
                own,
            });
        }
    }
    out
}

/// The send facts of own messages of one conversation that still have their `Message` row in
/// the outbox: message → [`Sending`]. Messages without an entry are [`Sending::Settled`].
///
/// `uploading`: the backend reports an upload of the message that is still moving (the
/// timeline query reads it from [`crate::runtime::Shared::uploading`]; a caller that cannot
/// stop anything, the delete commands, passes `false`: they refuse an in-flight message
/// whatever its uploads are).
pub(crate) fn sending_of(
    conn: &kourier_db::rusqlite::Connection,
    message: MessageId,
    uploading: bool,
) -> Result<Sending> {
    use kourier_db::rusqlite::OptionalExtension;
    let row: Option<(String, i64, i64)> = conn
        .prepare_cached(
            "SELECT state, attempts, cancel_state FROM outbox
             WHERE message_id = ?1 AND kind = 'Message'",
        )?
        .query_row([message.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((state, attempts, cancel_state)) = row else {
        return Ok(Sending::Settled);
    };
    let reached_by_anyone = || -> Result<bool> {
        Ok(crate::outbox::send_states(conn, message)?
            .values()
            .any(|s| !s.needs_send()))
    };
    if state == crate::outbox::TRANSMITTING {
        // The terms of the stop are read BEFORE this shortcut: an in-flight row that is a
        // retry or a resend to the members that failed is not stoppable. The recipients are
        // looked at only when the other terms hold (the answer is `false` otherwise).
        let stoppable = stoppable(StopFacts {
            transmitting: true,
            uploading,
            attempts,
            reached: uploading && attempts == 1 && reached_by_anyone()?,
            asked: cancel_state != 0,
        });
        return Ok(Sending::InFlight { stoppable });
    }
    let reached = reached_by_anyone()?;
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
    use kourier_model::{CannotSend, Support};

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
            registered: true,
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

    /// The stop rule is the AND of its terms (call 1 of the upload-progress step, and the
    /// mark of a stop that was asked, which greys the X out), and
    /// "Cancel Sending" is a take-back or a stop: a message in flight that is stoppable offers
    /// it and still no delete.
    #[test]
    fn a_stop_needs_all_its_terms_and_cancel_sending_is_a_take_back_or_a_stop() {
        let first = StopFacts {
            transmitting: true,
            uploading: true,
            attempts: 1,
            reached: false,
            asked: false,
        };
        assert!(stoppable(first));
        for (name, no) in [
            (
                "not in flight",
                StopFacts {
                    transmitting: false,
                    ..first
                },
            ),
            (
                "no upload reported",
                StopFacts {
                    uploading: false,
                    ..first
                },
            ),
            (
                "a retry",
                StopFacts {
                    attempts: 2,
                    ..first
                },
            ),
            (
                "no attempt on record",
                StopFacts {
                    attempts: 0,
                    ..first
                },
            ),
            (
                "a recipient has it",
                StopFacts {
                    reached: true,
                    ..first
                },
            ),
            (
                "the stop was asked already",
                StopFacts {
                    asked: true,
                    ..first
                },
            ),
        ] {
            assert!(!stoppable(no), "{name}");
        }

        let f = features();
        let flying = |stoppable| Facts {
            sending: Sending::InFlight { stoppable },
            ..own(&f)
        };
        let a = all(&flying(true), 0);
        assert!(a.cancel_sending, "a stop");
        assert!(
            !a.take_back && !a.delete_for_me && !a.delete_for_everyone,
            "a delete of a message in flight is refused whatever a stop could do"
        );
        assert!(!all(&flying(false), 0).cancel_sending);
        let unsent = |attempted| Facts {
            sending: Sending::Unsent { attempted },
            ..own(&f)
        };
        let a = all(&unsent(false), 0);
        assert!(a.cancel_sending && a.take_back, "a take-back");
        let a = all(&unsent(true), 0);
        assert!(
            !a.cancel_sending && !a.take_back,
            "a retry wait may be with the server"
        );
        assert!(!all(&own(&f), 0).cancel_sending, "a settled message");
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
            sending: Sending::InFlight { stoppable: false },
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
            sending: Sending::InFlight { stoppable: false },
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

    /// "Save As…" is offered for the files that are on this PC and for nothing else: a file
    /// that is not here has Download (his answer "Only once downloaded"), the rest of a long
    /// text is no file, and a message that is deleted, hidden on an unconfirmed clock or
    /// view-once (never saved, in any form) has none. One function answers the flag and the
    /// command; the command asks its two halves again on the attachment's row.
    #[test]
    fn save_offers_what_is_here_and_never_a_view_once_deleted_or_hidden_file() {
        use kourier_model::{AskFailure, Attachment, AttachmentId, AttachmentState, Body};
        let file = |id, state, long_text| Attachment {
            id: Some(AttachmentId(id)),
            state,
            long_text,
            ..Attachment::default()
        };
        let media = |atts: Vec<Attachment>| Body::Media {
            attachments: atts,
            caption: None,
        };
        let ids = |v: &[i64]| v.iter().map(|n| AttachmentId(*n)).collect::<Vec<_>>();
        let here = media(vec![file(1, AttachmentState::Local, false)]);
        assert_eq!(save_attachments(None, false, false, &here), ids(&[1]));
        // Every state but `Local`.
        for state in [
            AttachmentState::Remote,
            AttachmentState::Downloading { done: 1, total: 2 },
            AttachmentState::Failed { retryable: true },
            AttachmentState::Failed { retryable: false },
            AttachmentState::Expired,
            AttachmentState::Unavailable,
            AttachmentState::Asking,
            AttachmentState::AskFailed {
                why: AskFailure::NoAnswer,
            },
        ] {
            let body = media(vec![file(1, state, false)]);
            assert!(
                save_attachments(None, false, false, &body).is_empty(),
                "{state:?}"
            );
            assert!(may_save_file(state, false).is_err(), "{state:?}");
        }
        assert!(may_save_file(AttachmentState::Local, false).is_ok());
        // The message: view-once, a tombstone, hidden on an unconfirmed clock.
        assert!(
            save_attachments(None, true, false, &here).is_empty(),
            "view-once"
        );
        assert!(save_attachments(Some("ForEveryone"), false, false, &here).is_empty());
        assert!(save_attachments(Some("ForMe"), false, false, &here).is_empty());
        assert!(
            save_attachments(None, false, true, &here).is_empty(),
            "hidden"
        );
        let refusal = |r: Result<()>| r.expect_err("refused").refusal();
        assert_eq!(
            refusal(may_save_message(None, true, false)),
            Some(Refusal::WrongKind)
        );
        assert_eq!(
            refusal(may_save_message(Some("ForEveryone"), false, false)),
            Some(Refusal::Deleted)
        );
        assert_eq!(
            refusal(may_save_message(None, false, true)),
            Some(Refusal::Gone)
        );
        assert!(may_save_message(None, false, false).is_ok());
        // The rest of a long text is no file, a text has none, a sticker is one.
        let long = media(vec![file(1, AttachmentState::Local, true)]);
        assert!(save_attachments(None, false, false, &long).is_empty());
        assert_eq!(
            refusal(may_save_file(AttachmentState::Local, true)),
            Some(Refusal::WrongKind)
        );
        assert!(save_attachments(None, false, false, &Body::Text(Default::default())).is_empty());
        let sticker = Body::Sticker(Box::new(file(1, AttachmentState::Local, false)));
        assert_eq!(save_attachments(None, false, false, &sticker), ids(&[1]));
        // An album offers the pictures that are here, in order.
        let album = media(vec![
            file(1, AttachmentState::Local, false),
            file(2, AttachmentState::Remote, false),
            file(3, AttachmentState::Local, false),
            file(4, AttachmentState::Local, true),
        ]);
        assert_eq!(save_attachments(None, false, false, &album), ids(&[1, 3]));
        // An attachment without a row id cannot be named by a command.
        let nameless = media(vec![Attachment {
            state: AttachmentState::Local,
            ..Attachment::default()
        }]);
        assert!(save_attachments(None, false, false, &nameless).is_empty());
    }

    /// "Send Again" has one rule for the flag and the command: a reach-out refusal stops it,
    /// and so does a chat that cannot send (it lost the right after the message failed).
    #[test]
    fn a_reach_out_refusal_is_never_sent_again() {
        let refused = OutgoingState::Failed {
            reason: FailReason::ReachOutRefused,
            retryable: false,
        };
        let err = retry(Some(&refused), None).expect_err("refused");
        assert_eq!(err.refusal(), Some(Refusal::ReachOutRefused));
        // Whatever else a send can end in is tried again on request.
        for other in [
            OutgoingState::Failed {
                reason: FailReason::Network,
                retryable: true,
            },
            OutgoingState::Failed {
                reason: FailReason::Other("x".into()),
                retryable: false,
            },
            OutgoingState::Queued,
            OutgoingState::PartiallyFailed { failed: vec![] },
        ] {
            assert!(retry(Some(&other), None).is_ok(), "{other:?}");
        }
        assert!(retry(None, None).is_ok());
    }

    /// The one rule for "we cannot send in this chat" answers `may_send` (edit, delete, react),
    /// "Send Again" and a timer change alike: every reason refuses with `CannotSend`, no reason
    /// and no stated features do not, and the reach-out rule keeps its own refusal.
    #[test]
    fn a_chat_that_cannot_send_takes_no_send_again() {
        let failed = OutgoingState::Failed {
            reason: FailReason::Network,
            retryable: true,
        };
        let open = features();
        assert!(open.cannot_send.is_none());
        assert!(retry(Some(&failed), Some(&open)).is_ok());
        assert!(retry(Some(&failed), None).is_ok(), "NULL features send");
        assert!(cannot_send(Some(&open)).is_ok());
        assert!(cannot_send(None).is_ok());
        for why in [
            CannotSend::NotMember,
            CannotSend::AdminsOnly,
            CannotSend::NotAccepted,
        ] {
            let closed = ConversationFeatures {
                cannot_send: Some(why.clone()),
                ..features()
            };
            for floor in [Some(&failed), None] {
                let err = retry(floor, Some(&closed)).expect_err("refused");
                assert_eq!(err.refusal(), Some(Refusal::CannotSend), "{why:?}");
            }
            let err = cannot_send(Some(&closed)).expect_err("refused");
            assert_eq!(err.refusal(), Some(Refusal::CannotSend), "{why:?}");
        }
        // Both reasons at once: the network's refusal of a reach-out is the one that is told.
        let refused = OutgoingState::Failed {
            reason: FailReason::ReachOutRefused,
            retryable: false,
        };
        let closed = ConversationFeatures {
            cannot_send: Some(CannotSend::NotMember),
            ..features()
        };
        let err = retry(Some(&refused), Some(&closed)).expect_err("refused");
        assert_eq!(err.refusal(), Some(Refusal::ReachOutRefused));
    }

    // ---- the menu's pages of people (people pages P3) ----

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("id")
    }

    /// A text of `parts`: a piece of text, or a mention (None: the user's own placeholder).
    fn mention_text(
        parts: &[(&str, Option<&str>)],
        more: Vec<kourier_model::TextRange>,
    ) -> kourier_model::Body {
        use kourier_model::text::MENTION_PLACEHOLDER;
        use kourier_model::{RangeKind, Text, TextRange};
        let mut body = String::new();
        let mut ranges = Vec::new();
        for (text, mention) in parts {
            body.push_str(text);
            if let Some(user) = mention {
                ranges.push(TextRange {
                    start: u32::try_from(body.len()).expect("len"),
                    len: u32::try_from(MENTION_PLACEHOLDER.len_utf8()).expect("len"),
                    kind: RangeKind::Mention(uid(user)),
                });
                body.push(MENTION_PLACEHOLDER);
            }
        }
        ranges.extend(more);
        kourier_model::Body::Text(Text { body, ranges })
    }

    const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
    const ME_PNI: &str = "pni:00000000-0000-4000-8000-0000000000ab";
    const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
    const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";

    /// "Show Sender's Page": another person's message in a group, nothing else.
    #[test]
    fn the_sender_has_a_page_in_a_group_only_for_others() {
        assert!(
            show_sender(false, true),
            "another person's message in a group"
        );
        assert!(!show_sender(true, true), "the user's own message");
        assert!(
            !show_sender(false, false),
            "a message of a chat that is no group"
        );
        assert!(!show_sender(true, false));
    }

    /// One entry per distinct person, in the order of the first mention; the user's own ids
    /// are one entry; a text without a mention has none.
    #[test]
    fn a_menu_offers_each_mentioned_person_once() {
        let own = [uid(ME), uid(ME_PNI)];
        let none = mention_text(&[("nobody here", None)], Vec::new());
        assert!(show_people(false, &none, &own).is_empty());
        let body = mention_text(
            &[
                ("a ", Some(ALICE)),
                (" b ", Some(ME)),
                (" c ", Some(ALICE)),
                (" d ", Some(ME_PNI)),
                (" e ", Some(BOB)),
            ],
            Vec::new(),
        );
        assert_eq!(
            show_people(false, &body, &own),
            vec![
                ShownPerson {
                    user: uid(ALICE),
                    own: false
                },
                ShownPerson {
                    user: uid(ME),
                    own: true
                },
                ShownPerson {
                    user: uid(BOB),
                    own: false
                },
            ],
            "alice once, the user once (both his ids), bob"
        );
        // The ranges of a stored text need not be in order.
        let mut sorted = mention_text(&[("x ", Some(BOB)), (" y ", Some(ALICE))], Vec::new());
        if let kourier_model::Body::Text(t) = &mut sorted {
            t.ranges.reverse();
        }
        let people = show_people(false, &sorted, &own);
        assert_eq!(people[0].user, uid(BOB));
        assert_eq!(people[1].user, uid(ALICE));
    }

    /// A deleted message, a mention under a spoiler and a range that is no mention have no entry;
    /// a caption has its mentions like a text.
    #[test]
    fn a_menu_leaves_out_what_is_deleted_hidden_or_invalid() {
        use kourier_model::text::MENTION_PLACEHOLDER;
        use kourier_model::{RangeKind, TextRange};
        let own = [uid(ME)];
        let plain = mention_text(&[("hi ", Some(ALICE))], Vec::new());
        assert_eq!(show_people(false, &plain, &own).len(), 1);
        assert!(
            show_people(true, &plain, &own).is_empty(),
            "a deleted message"
        );
        // Bob's mention lies under a spoiler (the whole placeholder is inside it): no entry,
        // whatever the user saw; alice's, beside it, has one.
        let at =
            u32::try_from("a ".len() + MENTION_PLACEHOLDER.len_utf8() + " b ".len()).expect("len");
        let len = u32::try_from(MENTION_PLACEHOLDER.len_utf8()).expect("len");
        let hidden = mention_text(
            &[("a ", Some(ALICE)), (" b ", Some(BOB))],
            vec![TextRange {
                start: at - 1,
                len: len + 1,
                kind: RangeKind::Spoiler,
            }],
        );
        let people = show_people(false, &hidden, &own);
        assert_eq!(people.len(), 1, "{people:?}");
        assert_eq!(people[0].user, uid(ALICE));
        // A spoiler that only touches the mention does not hide it.
        let touching = mention_text(
            &[("a ", Some(ALICE)), (" b ", Some(BOB))],
            vec![TextRange {
                start: at + len,
                len: 1,
                kind: RangeKind::Spoiler,
            }],
        );
        assert_eq!(show_people(false, &touching, &own).len(), 2);
        // A mention range that is not exactly one placeholder is no mention.
        let invalid = kourier_model::Body::Text(kourier_model::Text {
            body: "no placeholder here".into(),
            ranges: vec![TextRange {
                start: 0,
                len: 2,
                kind: RangeKind::Mention(uid(ALICE)),
            }],
        });
        assert!(show_people(false, &invalid, &own).is_empty());
        // A caption of a media message has its mentions.
        let kourier_model::Body::Text(caption) = plain.clone() else {
            panic!("a text");
        };
        let media = kourier_model::Body::Media {
            attachments: Vec::new(),
            caption: Some(caption),
        };
        assert_eq!(show_people(false, &media, &own).len(), 1);
    }

    /// One person per placeholder, as the text shows it: two persons on one placeholder are the
    /// LAST one, "everyone" with a person is "everyone" (nobody) or the person by which comes last.
    #[test]
    fn a_menu_offers_the_one_person_the_text_shows_per_placeholder() {
        use kourier_model::text::MENTION_PLACEHOLDER;
        use kourier_model::{RangeKind, Text, TextRange};
        let own = [uid(ME)];
        let range = |kind| TextRange {
            start: 2,
            len: u32::try_from(MENTION_PLACEHOLDER.len_utf8()).expect("len"),
            kind,
        };
        let with = |kinds: Vec<RangeKind>| {
            kourier_model::Body::Text(Text {
                body: format!("a {MENTION_PLACEHOLDER}"),
                ranges: kinds.into_iter().map(range).collect(),
            })
        };
        let people = |kinds: Vec<RangeKind>| -> Vec<UserId> {
            show_people(false, &with(kinds), &own)
                .into_iter()
                .map(|p| p.user)
                .collect()
        };
        let (alice, bob) = (RangeKind::Mention(uid(ALICE)), RangeKind::Mention(uid(BOB)));
        assert_eq!(
            people(vec![alice.clone(), bob.clone()]),
            vec![uid(BOB)],
            "two ranges on one placeholder: the text shows the last, so the menu offers it"
        );
        assert_eq!(people(vec![bob.clone(), alice.clone()]), vec![uid(ALICE)]);
        assert!(
            people(vec![alice.clone(), RangeKind::MentionAll]).is_empty(),
            "\"everyone\" last: the text says @everyone, the menu lists nobody"
        );
        assert_eq!(
            people(vec![RangeKind::MentionAll, alice]),
            vec![uid(ALICE)],
            "a person last: the person"
        );
    }
}
