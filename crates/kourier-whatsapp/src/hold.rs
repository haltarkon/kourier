//! The receive hold of a link (W3a fix F5): while a login has no account yet, nothing reaches
//! `core_outbox`, so `AccountReady` always leads it.
//!
//! The core refuses, for good, an envelope that does not lead with `AccountReady` while a link is
//! open (`Envelope::validate_linking`), and the forwarder ships rows in `seq` order. The actor appends
//! `AccountReady` in its own turn after the pairing's signal, but the new connection (the 515
//! reconnect, the login, the first offline stanzas) runs on the client runtime meanwhile, and the
//! two writers of `core_outbox` that act on it - stage 2 (inbox rows -> events) and the handler's
//! [`EventWriter`](crate::events::EventWriter) (receipts) - could land a row below `AccountReady`'s
//! `seq`. The row would be quarantined for ever, and the server had it acknowledged already.
//!
//! So the actor CLOSES this hold when a login begins (before the client is made) and opens it when
//! the login has an account (`AccountReady` appended), or has ended without one:
//!
//! - stage 2 does not step while it is closed (the actor wakes it when it opens): the inbox rows
//!   stay `new`, durable, and become events behind `AccountReady`;
//! - the handler's writer WAITS while it is closed (bounded: [`PATIENCE`]), then writes; a wait that
//!   runs out fails the write like any other ([`WriteFailed::Held`](crate::events::WriteFailed)), so
//!   the stanza is not acknowledged and the server sends it again;
//! - the durability hook is not held: it commits only to the inbox (and its facts, which W3a has
//!   none of; a step that gives the hook `core_outbox` rows must hold them here too).
//!
//! A pairing that was REFUSED keeps the hold closed until the login is replaced: nothing of that
//! account's client reaches the core.
//!
//! The hold lives in the store handle (beside the app-state counters), so every clone of it, the
//! actor's, the handler's and stage 2's, sees the same state.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// How long a handler's write waits for the hold to open before it fails (the actor's turn after a
/// pairing is a few store jobs; this is a bound, not a pace).
#[cfg(not(test))]
pub(crate) const PATIENCE: Duration = Duration::from_secs(10);
#[cfg(test)]
pub(crate) const PATIENCE: Duration = Duration::from_millis(400);

#[derive(Debug, Default)]
struct State {
    closed: bool,
    /// Threads inside [`LinkHold::wait_open`] (tests read it).
    parked: usize,
}

/// See the module doc.
#[derive(Debug, Default)]
pub(crate) struct LinkHold {
    state: Mutex<State>,
    opened: Condvar,
}

impl LinkHold {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Closes or opens the hold; opening releases every waiter.
    pub(crate) fn set_closed(&self, closed: bool) {
        let mut state = self.lock();
        state.closed = closed;
        if !closed {
            self.opened.notify_all();
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// BLOCKS the calling thread until the hold is open or `limit` has passed; `true` when it is
    /// open. For a thread of the client runtime that already waits for a commit (the handler), never
    /// for the actor or a task that serves the store.
    pub(crate) fn wait_open(&self, limit: Duration) -> bool {
        let mut state = self.lock();
        if !state.closed {
            return true;
        }
        state.parked += 1;
        let (mut state, _) = self
            .opened
            .wait_timeout_while(state, limit, |state| state.closed)
            .unwrap_or_else(|e| e.into_inner());
        state.parked -= 1;
        !state.closed
    }

    /// Threads waiting in [`wait_open`](Self::wait_open) now.
    #[cfg(test)]
    pub(crate) fn parked(&self) -> usize {
        self.lock().parked
    }
}

#[cfg(test)]
mod tests;
