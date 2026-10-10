//! The contract-conformance rules (§10.2): `Envelope::validate` plus the rules that need the
//! account's own ids, and the rules of an edited history message
//! (`IncomingMessage::edits`). Every envelope the fake ships is checked (during a link with
//! `Envelope::validate_linking`: `AccountReady` first); the per-network scripts run through the
//! same checks statically.
//!
//! What the suite cannot check of a history batch, because only the backend knows it: that
//! `read: false` is said only for a message the backend KNOWS to be unread (the core counts
//! it), and that `ConversationPatch::unread_hint` counts only unread messages it has no rows
//! for. The scripts state it where they set the two.

use std::collections::BTreeSet;

use kourier_backend_api::{
    BackendEvent, ContractViolation, Envelope, IncomingMessage, MAX_HISTORY_EDITS,
};
use kourier_model::{Body, Origin, UserId};

/// A broken contract rule. Log-safe.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Violation {
    /// A rule `Envelope::validate` enforces (acks, history batch size, id grammar).
    #[error(transparent)]
    Contract(#[from] ContractViolation),
    /// An own message carries an author (own messages have `author: None`, §3.1).
    #[error("own message sent with an author in {0}")]
    OwnMessageWithAuthor(&'static str),
    /// A modifier by our own account names an own id instead of `by: None`.
    #[error("own modifier sent with an author in {0}")]
    OwnModifierWithAuthor(&'static str),
    /// The versions of an edited message (`IncomingMessage::edits`) break their rule: they
    /// belong to a message of a history batch that is no notice, are at most
    /// `MAX_HISTORY_EDITS`, oldest first, each under an id of its own that is not the
    /// message's.
    #[error("edits of a history message: {0}")]
    HistoryEdits(&'static str),
}

/// The rules of `IncomingMessage::edits` (the ids' grammar is `Envelope::validate`'s).
fn check_edits(m: &IncomingMessage, history: bool) -> Result<(), Violation> {
    if m.edits.is_empty() {
        return Ok(());
    }
    if !history {
        // The core does not read them there: a live edit is `BackendEvent::Edited`.
        return Err(Violation::HistoryEdits("on a live message"));
    }
    if matches!(m.body, Body::System(_)) {
        return Err(Violation::HistoryEdits("on a notice or a call line"));
    }
    if m.edits.len() > MAX_HISTORY_EDITS {
        return Err(Violation::HistoryEdits("more than the core reads"));
    }
    let mut seen = BTreeSet::from([&m.local]);
    if !m.edits.iter().all(|edit| seen.insert(&edit.local)) {
        return Err(Violation::HistoryEdits(
            "a version under the message's own id, or two under one id",
        ));
    }
    if !m.edits.is_sorted_by_key(|edit| edit.at) {
        return Err(Violation::HistoryEdits("not oldest first"));
    }
    Ok(())
}

fn check_message(
    m: &IncomingMessage,
    own: &[UserId],
    event: &'static str,
    history: bool,
) -> Result<(), Violation> {
    if let Origin::Remote { author, .. } = &m.origin
        && own.contains(author)
    {
        return Err(Violation::OwnMessageWithAuthor(event));
    }
    check_edits(m, history)
}

/// Checks one event against the rules that need our own ids.
pub fn check_event(e: &BackendEvent, own: &[UserId]) -> Result<(), Violation> {
    let own_by = |by: &Option<UserId>| by.as_ref().is_some_and(|b| own.contains(b));
    match e {
        BackendEvent::Message(m) => check_message(m, own, e.name(), false),
        BackendEvent::HistoryBatch { messages, .. } => messages
            .iter()
            .try_for_each(|m| check_message(m, own, e.name(), true)),
        BackendEvent::Edited { by, .. }
        | BackendEvent::Deleted { by, .. }
        | BackendEvent::Reaction { by, .. }
            if own_by(by) =>
        {
            Err(Violation::OwnModifierWithAuthor(e.name()))
        }
        _ => Ok(()),
    }
}

/// Checks an envelope: `Envelope::validate`, then every event against `own`.
pub fn check_envelope(env: &Envelope, own: &[UserId]) -> Result<(), Violation> {
    env.validate()?;
    env.events.iter().try_for_each(|e| check_event(e, own))
}
