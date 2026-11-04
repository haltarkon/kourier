//! The server's acks of the messages Kourier sends (W5a, decisions D5/D6; brief §2.3, critic H1).
//!
//! `Client::send` returns when the stanza is WRITTEN, not when the server took it: the answer is the
//! `<ack class="message" id=.. [t=..] [error=..]>` that follows, which upstream hands to the event handler as
//! `Event::ServerAck` (`translate::message_ack` reads it) and resolves nothing else with. This registry is where
//! the handler hands it to the send that waits for it: a sync map (the handler never waits, it runs on upstream's
//! tasks), one per ACCOUNT (the actor makes it once and wires every client of the account to it, like the receive
//! path), keyed by the stanza id, which is the core's local (decision D2).
//!
//! ORDER. A send registers its wait BEFORE `Session::send` ([`Acks::wait`]): the ack can beat the send's
//! future. A wait whose frame never left (a failure before the wire) is withdrawn ([`Acks::withdraw`]).
//!
//! ONE FRAME PER LOCAL PER CONNECTION (D5). The registry counts the connections ([`Acks::connection_ended`],
//! told by the handler at upstream's `Disconnected`, and by the actor when it stops a client): every wait belongs
//! to the connection that was live when it was made, and the end of a connection ends its waits
//! ([`Waited::Ended`]). A wait for a local that already has one on the live connection is NOT first
//! ([`Wait::first`]): its frame was written (or is being written) and not acked yet, so the caller sends no
//! frame and waits for that ack. EVERY answer is REMEMBERED for the connection (fix F1; it amends D21): an ack and
//! a 463 or 475 for the rest of it (so a failed ledger commit, or an ack that came after its wait gave up, never leads to
//! a second frame on the connection), a nack for its FLOOR ([`Nack::floor`]: 429 five minutes, 421, 500 and 503
//! thirty seconds) or, when it has none, for the rest of the connection. A later wait of that local hears the
//! remembered answer at once and is not first (no frame; [`Wait::left`] is the time left of the floor, the
//! `retry_after` of the failure it answers), so the core's own retry of a retryable nack cannot make a burst of
//! frames of one local. After the floor ONE new frame may go out (the nack is known, so no duplicate is possible):
//! that wait is first, and after a 421 (the group's addressing was stale) it asks for fresh group metadata
//! ([`Wait::refresh_groups`]). A nack never replaces a remembered ack, 463 or 475.
//!
//! THE LAG (fix F12, known and safe). Upstream reconnects BY ITSELF (the keepalive watchdog, the dead-socket timer, a
//! critical app-state timeout) and dispatches no `Disconnected` for it, so the registry's number can stay the old
//! socket's while a new one is up: a frame written on the new socket is registered under the old number. It cannot be
//! made to follow the new socket at the pin: the `<success>` that authenticates it is a connection-critical stanza that
//! is never offered to an interceptor (`node_io.rs` `is_connection_critical`), and upstream dispatches `Connected` only
//! AFTER the offline wait, when a Transmit may already have written a frame on the new socket under the old number;
//! ending the connection AT `Connected` would end that frame's wait and the core's resend would be `first` again on the
//! SAME socket (a second frame). So the handler ends the connection on `Disconnected` only (test
//! `session::client::tests::a_connected_event_does_not_end_the_registrys_connection`), and the lag can only write FEWER
//! frames (the registry holds both sockets as one connection); the cost is one extra reconnect when a wait of a frame
//! the old socket lost times out ([`Acks::claim_end`]).
//!
//! NO ACK IN TIME (D5, chunk 9b). A wait that heard nothing within `ACK_WAIT` ends its connection: the first such
//! wait of the live connection claims the end ([`Acks::claim_end`]), the session drops the connection, and
//! [`Acks::end`] ends it here (only if it is still the live one: a connection that ended meanwhile, and the one
//! after it, are never ended by a claim of an older one). A late ack that comes before the end still reaches the
//! wait that gave up its reply: the attempt keeps listening ([`Wait::hear`]) and records it.
//!
//! Nothing here logs: an id is the core's local, and an ack carries nothing that names a person (its `from`, the
//! chat, is not read at all: upstream correlates an ack by its id alone, and so does this).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::ledger::Outcome;

/// The `error` of a nack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nack {
    /// The code, when it is a number (`463`, `479`, ...).
    Code(u16),
    /// A code that is no number: kept as "unknown", never as text.
    Unreadable,
}

/// How long a 429 keeps its local from being written again on the same connection (fix F1).
pub(crate) const RATE_LIMITED_FLOOR: Duration = Duration::from_secs(5 * 60);

/// How long a 421, a 500 or a 503 keeps its local from being written again on the same connection (fix F1).
pub(crate) const TRANSIENT_FLOOR: Duration = Duration::from_secs(30);

impl Nack {
    /// The time the nack keeps its local from being written again on the same connection (fix F1); `None`: for
    /// the rest of the connection (every other nack, the 463 included).
    pub(crate) fn floor(self) -> Option<Duration> {
        match self {
            Self::Code(429) => Some(RATE_LIMITED_FLOOR),
            Self::Code(421 | 500 | 503) => Some(TRANSIENT_FLOOR),
            Self::Code(_) | Self::Unreadable => None,
        }
    }

    /// Whether the server said the group's addressing was stale: the next frame asks for fresh group metadata.
    fn stale_group(self) -> bool {
        matches!(self, Self::Code(421))
    }
}

/// The ack of one message stanza, as `translate::message_ack` reads it. No `Debug`: the id is the core's local.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Ack {
    /// The stanza id (the core's local).
    pub(crate) id: String,
    /// What the server said.
    pub(crate) answer: Answer,
}

/// What the server said to one stanza.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// It took it; `server_t` is the ack's `t` in seconds when it carried a readable one.
    Acked { server_t: Option<i64> },
    /// It refused it.
    Nacked(Nack),
}

impl Answer {
    /// What the ledger records of this answer (D4, critic H2): an ack, and a 463 or a 475 as `refused`. Every other
    /// nack is NOT recorded: the server does not have the stanza, so Send Again may go. 475 is upstream's
    /// `NewChatMessagesCapped` (`wacore/src/protocol/nack.rs` at the pin): a cap on messages to new chats that
    /// counts against the same account, so it is met like a 463 (never retried) until a real run shows what it
    /// means on our acks (fix F8).
    pub(crate) fn recorded(self) -> Option<Outcome> {
        match self {
            Self::Acked { server_t } => Some(Outcome::Acked { server_t }),
            Self::Nacked(Nack::Code(463 | 475)) => Some(Outcome::Refused),
            Self::Nacked(_) => None,
        }
    }
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Waited {
    /// The server's answer.
    Answered(Answer),
    /// No answer will come on this wait's connection: it ended, or the frame was withdrawn (never left).
    Ended,
}

/// Whether [`Acks::answer`] found anybody waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Heard {
    /// At least one wait of the live connection got the answer.
    Waiter,
    /// Nobody waited for this id (every wait of it gave up, or it was never ours).
    Nobody,
}

/// One registered wait (see [`Acks::wait`]).
pub(crate) struct Wait {
    first: bool,
    connection: u64,
    /// For a remembered nack still inside its floor: the time left of it (fix F1).
    left: Option<Duration>,
    /// The first wait after a 421's floor: the frame asks for fresh group metadata (fix F1).
    refresh_groups: bool,
    /// `None` once it gave its answer (a oneshot is polled to its end once).
    answer: Option<oneshot::Receiver<Answer>>,
}

impl Wait {
    /// `true`: no other wait of this local on the live connection, so the caller writes the frame. `false`: a
    /// frame of this local was written on this connection and is not answered yet: the caller writes NOTHING and
    /// waits for that answer (D5).
    pub(crate) fn first(&self) -> bool {
        self.first
    }

    /// The connection this wait belongs to (the count [`Acks::connection`] gives).
    pub(crate) fn connection(&self) -> u64 {
        self.connection
    }

    /// For a wait that heard a REMEMBERED nack inside its floor: the time left of the floor. `None` for a live
    /// answer (the nack's whole floor then) and for a nack without one.
    pub(crate) fn left(&self) -> Option<Duration> {
        self.left
    }

    /// Whether the frame of this (first) wait asks upstream for fresh group metadata: the last answer of the local
    /// was a 421 (fix F1).
    pub(crate) fn refresh_groups(&self) -> bool {
        self.refresh_groups
    }

    /// The answer, or [`Waited::Ended`] when the connection ended or the frame was withdrawn.
    #[cfg(test)]
    pub(crate) async fn answered(mut self) -> Waited {
        self.hear().await
    }

    /// [`answered`](Self::answered) without giving the wait up: a wait that timed out can still hear a late
    /// answer (D5). After it gave its answer once it gives [`Waited::Ended`].
    pub(crate) async fn hear(&mut self) -> Waited {
        let Some(answer) = self.answer.as_mut() else {
            return Waited::Ended;
        };
        let heard = match answer.await {
            Ok(answer) => Waited::Answered(answer),
            Err(_) => Waited::Ended,
        };
        self.answer = None;
        heard
    }
}

#[derive(Default)]
struct Inner {
    /// The live connection's number: how many connections ended before it.
    connection: u64,
    /// The waits of the live connection, by stanza id.
    waits: HashMap<String, Vec<oneshot::Sender<Answer>>>,
    /// The answers of the live connection, by stanza id (fix F1): a later wait of such a local hears it at once
    /// and writes nothing while it holds (D21: at most one frame per local per connection, also when the
    /// ledger's commit failed or the answer came after its wait gave up, and one more per floor after a nack).
    answered: HashMap<String, Remembered>,
    /// The live connection's end was claimed ([`Acks::claim_end`]) and is under way.
    ending: bool,
}

/// One remembered answer.
struct Remembered {
    answer: Answer,
    /// Until when it holds: `None` for the rest of the connection.
    until: Option<Instant>,
}

/// What the memory says to a new wait of a local.
enum Recall {
    /// Nothing is remembered: the wait is first unless a frame is in the air.
    Nothing,
    /// The remembered answer holds: no frame; `left` is the rest of its floor.
    Holds {
        answer: Answer,
        left: Option<Duration>,
    },
    /// A nack whose floor is over: one new frame may go out; `stale_group` after a 421.
    FloorOver { stale_group: bool },
}

/// The registry (one per account).
#[derive(Default)]
pub(crate) struct Acks {
    inner: Mutex<Inner>,
    /// Tests move the registry's clock by this much (the floors are minutes; a test does not wait them).
    #[cfg(test)]
    skew: Mutex<Duration>,
}

impl Inner {
    /// What is remembered of `id` at `now`.
    fn recall(&self, id: &str, now: Instant) -> Recall {
        let Some(remembered) = self.answered.get(id) else {
            return Recall::Nothing;
        };
        match remembered.until {
            Some(until) if until <= now => Recall::FloorOver {
                stale_group: matches!(remembered.answer, Answer::Nacked(nack) if nack.stale_group()),
            },
            Some(until) => Recall::Holds {
                answer: remembered.answer,
                left: Some(until.saturating_duration_since(now)),
            },
            None => Recall::Holds {
                answer: remembered.answer,
                left: None,
            },
        }
    }

    /// Keeps the answer of `ack` for the connection; a nack the ledger does not record never replaces an ack or
    /// a 463 (the ledger's word stands).
    fn remember(&mut self, ack: &Ack, now: Instant) {
        let recorded_kept = self
            .answered
            .get(&ack.id)
            .is_some_and(|kept| kept.answer.recorded().is_some());
        if recorded_kept && ack.answer.recorded().is_none() {
            return;
        }
        let until = match ack.answer {
            Answer::Nacked(nack) => nack.floor().map(|floor| now + floor),
            Answer::Acked { .. } => None,
        };
        self.answered.insert(
            ack.id.clone(),
            Remembered {
                answer: ack.answer,
                until,
            },
        );
    }
}

impl Acks {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while the lock was held leaves nothing half-written (every change is one map operation).
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The registry's time (the floors count in it).
    fn now(&self) -> Instant {
        #[cfg(test)]
        {
            Instant::now()
                + *self
                    .skew
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
        #[cfg(not(test))]
        {
            Instant::now()
        }
    }

    /// Moves the registry's clock on (tests).
    #[cfg(test)]
    pub(crate) fn advance(&self, by: Duration) {
        *self
            .skew
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += by;
    }

    /// Registers a wait for the ack of `id` on the live connection. Call it BEFORE the frame is written.
    pub(crate) fn wait(&self, id: &str) -> Wait {
        let now = self.now();
        let (tx, answer) = oneshot::channel();
        let mut inner = self.inner();
        let connection = inner.connection;
        let mut stale_group = false;
        match inner.recall(id, now) {
            Recall::Holds {
                answer: heard,
                left,
            } => {
                // The receiver is alive: this send cannot fail.
                let _ = tx.send(heard);
                return Wait {
                    first: false,
                    connection,
                    left,
                    refresh_groups: false,
                    answer: Some(answer),
                };
            }
            Recall::FloorOver { stale_group: stale } => stale_group = stale,
            Recall::Nothing => {}
        }
        let waits = inner.waits.entry(id.to_owned()).or_default();
        let first = waits.is_empty();
        waits.push(tx);
        Wait {
            first,
            connection,
            left: None,
            refresh_groups: first && stale_group,
            answer: Some(answer),
        }
    }

    /// The frame of `id` did not leave on `connection` (a failure before the wire): its waits end, so the
    /// next wait of `id` is first again. A wait of an older connection withdraws nothing of the live one.
    pub(crate) fn withdraw(&self, id: &str, connection: u64) {
        let mut inner = self.inner();
        if inner.connection == connection {
            inner.waits.remove(id);
        }
    }

    /// The server's answer to `ack.id`: every wait of it gets it and is gone; the answer is kept (an ack and a
    /// 463 for the rest of the connection, a nack for its floor or the rest of the connection: a later wait of
    /// the local hears it with no frame). Called by the event handler; never waits.
    pub(crate) fn answer(&self, ack: Ack) -> Heard {
        let now = self.now();
        let mut inner = self.inner();
        inner.remember(&ack, now);
        let Some(waits) = inner.waits.remove(&ack.id) else {
            return Heard::Nobody;
        };
        drop(inner);
        let mut heard = Heard::Nobody;
        for wait in waits {
            if wait.send(ack.answer).is_ok() {
                heard = Heard::Waiter;
            }
        }
        heard
    }

    /// The live connection ended: every wait of it ends ([`Waited::Ended`]) and the next connection begins.
    pub(crate) fn connection_ended(&self) {
        Self::next_connection(&mut self.inner());
    }

    fn next_connection(inner: &mut Inner) {
        inner.connection += 1;
        inner.waits.clear();
        inner.answered.clear();
        inner.ending = false;
    }

    /// A wait of `connection` heard nothing within `ACK_WAIT` (D5): `true` for the ONE caller that is to end it
    /// (the connection is still the live one and nobody claimed its end yet); `false` when it ended already or
    /// another wait is ending it.
    pub(crate) fn claim_end(&self, connection: u64) -> bool {
        let mut inner = self.inner();
        if inner.connection != connection || inner.ending {
            return false;
        }
        inner.ending = true;
        true
    }

    /// The claimed end of `connection` is done (the session dropped it): its waits end and the next connection
    /// begins, unless `connection` is no longer the live one (it ended another way meanwhile).
    pub(crate) fn end(&self, connection: u64) {
        let mut inner = self.inner();
        if inner.connection == connection {
            Self::next_connection(&mut inner);
        }
    }

    /// How many waits of the live connection are registered for `id` (the tests wait for a resend to have joined the
    /// wait of the connection that is ending, instead of sleeping for a while).
    #[cfg(test)]
    pub(crate) fn waiting(&self, id: &str) -> usize {
        self.inner().waits.get(id).map_or(0, Vec::len)
    }

    /// The live connection's number (the tests read it).
    #[cfg(test)]
    pub(crate) fn connection(&self) -> u64 {
        self.inner().connection
    }
}

#[cfg(test)]
mod tests;
