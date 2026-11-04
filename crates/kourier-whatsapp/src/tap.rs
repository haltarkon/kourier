//! [`Tap`]: where a client's signals enter the actor (W2, chunk C4a) - the Forwarder's pure half.
//!
//! TWO LINES JUDGE A PAIRING (W2c). The FIRST is the stanza claim (`prepair`): it reads the `<pair-success>`
//! before upstream does and asks [`Tap::judge_pairing`], so a refused pairing never makes a device: no
//! store write, no `pair-device-sign`, no 515, nothing to log out. It tells the login with
//! [`Tap::deliver_claimed`], which gets past a refused gate (the claim sets the flag itself). The SECOND
//! is the gate below, on the event that follows a pairing upstream completed; it matters only if the
//! claim did not see the pairing (a pin bump moved it).
//!
//! The client's event handler (`RealSession`) translates an upstream event into a [`WaSignal`]
//! and hands it to the account's `Tap`; the test double calls the `Tap` by hand. The `Tap` does the
//! one thing that must happen at the moment the event is delivered, in the handler and not after a
//! hop to the actor (critic H7, decision L6): the pairing's identity checks. With a [`Gate`] (a
//! login is running) a `PairSuccess` is decided here; a refusal sets the gate's flag, and from then
//! on every later event of that client is dropped. THE GATE STOPS KOURIER, NOT UPSTREAM (review F2; this holds
//! for the second line only):
//! it keeps Kourier from acting on a refused account's events (nothing is recorded, nothing is told
//! to the core, no `AccountReady`), but upstream's client goes on running until its logout reaches
//! the server - the 515 reconnect, `<success>`, its post-login work (the critical app-state sync into
//! this account's `backend.db`, offline messages decrypted and acknowledged with delivery
//! receipts). The window is closed by the logout, which goes on the first authenticated socket (not
//! after full session readiness) and is bounded at ~20 s; see `session::client` and the build
//! report's risks. The signal is still handed on, so the machine ends the login; the caller of
//! [`Tap::deliver`] learns of the refusal ([`Delivered::Refused`]) and starts that logout at once.
//!
//! The channel is unbounded on purpose: W2's kinds are rare (a few per minute at most), and a
//! handler that blocks stalls the client's own read loop.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::link::{Decision, Gate, Refused, Verdict};
use crate::translate::{Paired, WaSignal};

/// What became of one signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivered {
    /// Handed to the actor.
    Sent,
    /// Dropped: the pairing of this client was refused earlier, or the actor no longer listens.
    Dropped,
    /// This was the pairing and the gate refused it. It is handed to the actor too.
    Refused,
}

/// The sender side of one client's signals. Cheap to clone.
#[derive(Debug, Clone)]
pub(crate) struct Tap {
    tx: mpsc::UnboundedSender<WaSignal>,
    gate: Option<Arc<Gate>>,
}

impl Tap {
    /// A tap and the actor's end of its channel. `gate` is the running login's (none for a client
    /// that only connects).
    pub(crate) fn channel(gate: Option<Arc<Gate>>) -> (Self, mpsc::UnboundedReceiver<WaSignal>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx, gate }, rx)
    }

    /// Whether the login's gate refused a pairing of this client: nothing it receives may be kept
    /// (W4: the history of a refused account is never captured).
    pub(crate) fn is_refused(&self) -> bool {
        self.gate.as_ref().is_some_and(|gate| gate.is_refused())
    }

    /// Hands `signal` to the actor, after the gate's decision on a pairing.
    pub(crate) fn deliver(&self, signal: WaSignal) -> Delivered {
        let mut refused = false;
        if let Some(gate) = &self.gate {
            if gate.is_refused() {
                return Delivered::Dropped;
            }
            if let WaSignal::PairSuccess(paired) = &signal {
                refused = matches!(gate.decide(paired), Decision::Refuse(_));
            }
        }
        match (self.tx.send(signal), refused) {
            (Err(_), _) => Delivered::Dropped,
            (Ok(()), true) => Delivered::Refused,
            (Ok(()), false) => Delivered::Sent,
        }
    }

    /// The first line (W2c): the verdict on the `pair-success` children of a claimed `<iq>`. No gate
    /// (a client that only connects, the one that logs a device out, a rebuilt one) means no login
    /// can take a pairing: refused. A refusal sets the gate's flag (except after a pairing was let
    /// through, see [`Gate::claim`]).
    pub(crate) fn judge_pairing(&self, pairs: &[Option<Paired>]) -> Verdict {
        match &self.gate {
            None => Verdict::Refuse(Refused::no_login()),
            Some(gate) => gate.claim(pairs),
        }
    }

    /// Tells the login machine that a pairing was refused before its device existed. The ONE signal
    /// that gets past a refused gate (the claim set the flag itself, moments ago, and the machine still
    /// has to end the login); every other signal stays dropped.
    pub(crate) fn deliver_claimed(&self, paired: Option<Paired>) -> Delivered {
        match self.tx.send(WaSignal::PairClaimed(paired)) {
            Ok(()) => Delivered::Sent,
            Err(_) => Delivered::Dropped,
        }
    }
}

#[cfg(test)]
mod tests;
