//! [`Facts`]: what the actor tells besides the connection state it publishes itself (W2, chunk
//! C4a).
//!
//! The actor publishes the [`ConnState`](kourier_model::ConnState) of a signal itself, and since K-A
//! that state carries a ban's kind, a logout's cause and WhatsApp's own note. What is still no part
//! of it is the reach-out lock, an [`AccountHold`], which the contract states as
//! `BackendEvent::AccountHold`, not as a connection state. The actor hands both to this trait, and
//! since W5a (chunk 10, decision D8) it sends the hold to the core itself, as the contract's ephemeral
//! `AccountHold`, right after it hands it here (`Actor::state_hold`). [`LogFacts`] logs the kinds and
//! nothing else; the actor's tests record what reaches it.

use kourier_model::AccountHold;

use crate::states::WaState;

/// Where the actor tells the typed facts. Called on the actor's thread, in order.
pub(crate) trait Facts: Send + 'static {
    /// The connection changed to `state` (also called for a state with no extra fact).
    fn state(&mut self, state: &WaState);
    /// The core's statement about the reach-out lock: a hold, or `None` for no hold. Called once at
    /// every connect, and when the network pushes a change.
    fn hold(&mut self, hold: Option<AccountHold>);
}

/// The production implementation: a log line per fact, kinds only (the core gets the hold from the actor).
pub(crate) struct LogFacts;

impl Facts for LogFacts {
    fn state(&mut self, state: &WaState) {
        // `ConnState`'s `Debug` names the ban's kind and the logout's cause; the network's note is
        // in it too, and `NetworkNote`'s `Debug` says only which parts are there, never the text
        // or the page (test `the_state_is_logged_with_its_kind_and_cause_and_without_the_networks_words`).
        tracing::info!(
            conn = ?state.conn,
            terminal = state.terminal,
            "whatsapp connection state",
        );
    }

    fn hold(&mut self, hold: Option<AccountHold>) {
        tracing::info!(active = hold.is_some(), "whatsapp reach-out lock");
    }
}

#[cfg(test)]
mod tests;
