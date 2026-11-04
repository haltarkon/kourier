//! The reach-out lock as the SENDS see it (W5a, decision D8; fix F6).
//!
//! The actor owns what it knows of the lock (the last statement, when the run first asked) and shares it through a
//! [`HoldView`], so an attempt (or a typing state) that passed the gate at its start can ask AGAIN right before it
//! writes ([`Recheck`]): between the gate and the frame the attempt awaits the store, the account's LID mode, a
//! group's addressing and, inside upstream's send, usync and prekey IQs, and a lock pushed meanwhile (`WaSignal::Hold`,
//! earned by another frame) must stop this frame too (D8: "a Transmit is answered Transient ... while a hold is
//! known"). The check is PURE: the time it judges by is the caller's.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_model::AccountHold;

use super::attempt::Gate;

/// What the actor knows of the lock, and the end of "unknown".
#[derive(Clone)]
pub(crate) struct HoldView {
    facts: Arc<Mutex<Facts>>,
    /// After this long since the run first asked, "unknown" counts as "no hold" (D8, critic H3).
    unknown_end: Duration,
}

#[derive(Default)]
struct Facts {
    /// The last statement: `Some(Some(hold))` a hold, `Some(None)` none, `None` unknown.
    last: Option<Option<AccountHold>>,
    /// When the first pull of the run was asked (the actor's clock, ms).
    asked_ms: Option<u64>,
}

impl HoldView {
    pub(crate) fn new(unknown_end: Duration) -> Self {
        Self {
            facts: Arc::default(),
            unknown_end,
        }
    }

    /// The statement the actor made (a push or a pull's answer).
    pub(crate) fn set(&self, last: Option<AccountHold>) {
        self.facts.lock().expect("lock").last = Some(last);
    }

    /// The last statement: `None` while the lock is unknown.
    pub(crate) fn last(&self) -> Option<Option<AccountHold>> {
        self.facts.lock().expect("lock").last
    }

    /// The run asked for the lock now; only the FIRST ask of the run counts.
    pub(crate) fn asked(&self, now_ms: u64) {
        self.facts
            .lock()
            .expect("lock")
            .asked_ms
            .get_or_insert(now_ms);
    }

    /// What the lock lets a send do at `now_ms`: a known lock in force holds it (it wins over the end of
    /// "unknown"); a known "none" opens; an unknown one opens only [`Self::unknown_end`] after the first ask.
    pub(crate) fn gate(&self, now_ms: u64) -> Gate {
        let facts = self.facts.lock().expect("lock");
        match facts.last {
            Some(Some(hold)) if hold.until_ms.is_none_or(|until| until > now_ms) => Gate::Held,
            Some(_) => Gate::Open,
            None => {
                let end = u64::try_from(self.unknown_end.as_millis()).unwrap_or(u64::MAX);
                let ended = facts
                    .asked_ms
                    .is_some_and(|asked| now_ms.saturating_sub(asked) >= end);
                if ended { Gate::Open } else { Gate::Unknown }
            }
        }
    }
}

/// The gate asked again: what an attempt or a typing state holds to look at the lock once more before it writes.
#[derive(Clone)]
pub(crate) struct Recheck(Arc<dyn Fn() -> Gate + Send + Sync>);

impl Recheck {
    pub(crate) fn new(gate: impl Fn() -> Gate + Send + Sync + 'static) -> Self {
        Self(Arc::new(gate))
    }

    /// A gate that never changes (tests).
    #[cfg(test)]
    pub(crate) fn fixed(gate: Gate) -> Self {
        Self::new(move || gate)
    }

    /// The gate now.
    pub(crate) fn gate(&self) -> Gate {
        (self.0)()
    }

    /// Whether a frame may be written now.
    pub(crate) fn open(&self) -> bool {
        self.gate() == Gate::Open
    }
}
