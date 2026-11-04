//! What the core reads of a network's descriptor when it sends (K-B1, architecture §3.1, §3.5.3
//! and §4.8): one `Copy` value made once where a supervisor starts, so the dispatcher and the
//! outbox functions it calls carry a network's rules and not one loose field after the other.
//!
//! - [`NetRules::scope`]: how message ids are scoped (what ingest and the outbox's lookups
//!   need);
//! - [`NetRules::edit_target`]: which message an own edit names on the wire
//!   ([`EditTarget`]), used when an edit is typed ([`crate::modify`]) and when it is sent
//!   ([`crate::outbox::take_due`]);
//! - [`NetRules::shutdown_grace`]: how long a stop gives the account's actor, when the network
//!   states more than the core's default (R-Q1, [`Timeouts::grace_of`](crate::Timeouts::grace_of));
//! - [`NetRules::history_read`]: what the `read` flag of a history row means (W4 fix F1,
//!   [`HistoryReadState`]); where it is unknown a history copy never raises an existing row;
//! - [`NetRules::outbox`]: the retry budget, the back-off cap and the challenge lifetime
//!   ([`OutboxPolicy`], architecture §4.8). Every site of the outbox reads it: the deadline a
//!   row gets (`store`, `modify::queue`, `retry`), the wait that is read as "due" (`take_due`),
//!   the back-off (`release`, `classify`, `record_outcome`, `modify::record_outcome`), the
//!   time a hold gives back (`give_back_held_time`) and the life of a stored challenge
//!   (`StoredChallenge::is_fresh`). None reads the constants `outbox::BUDGET` and
//!   `outbox::MAX_BACKOFF`: they are only the default policy's values;
//! - [`NetRules::server`]: what the network's server owns (KT-2, [`ServerRules`]): every site
//!   that has the rules in hand begins its `Apply` under them
//!   ([`Apply::under_rules`](crate::ingest::Apply::under_rules)).
//!
//! A `NetRules` is made where a network is registered (the supervisor, the sites of
//! `runtime` that need a live network). The few paths that run for an account whose network
//! is not registered (a paused account: the take-back `cancel_message`, the derive spawn) keep
//! the scope-only fallback of `MsgIdScope::default()`; they read no policy and no edit
//! target, and do no lookup by message key (K-B1 critic LOW 9, checked).

use std::time::Duration;

use kourier_model::{
    EditTarget, HistoryReadState, MsgIdScope, NetworkDescriptor, OutboxPolicy, ServerRules,
};

/// The rules of one network, from its descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NetRules {
    /// How message ids are scoped.
    pub scope: MsgIdScope,
    /// Which message an own edit names on the wire.
    pub edit_target: EditTarget,
    /// The outbox's budget, back-off cap and challenge lifetime.
    pub outbox: OutboxPolicy,
    /// What the `read` flag of a history row means: where it is
    /// [`Unknown`](HistoryReadState::Unknown) a history copy never raises an existing row's
    /// read state (W4 fix F1).
    pub history_read: HistoryReadState,
    /// How long the network asks for its actor to stop on its own at a quit; `None` is the
    /// core's default ([`Timeouts::shutdown_grace`](crate::Timeouts::shutdown_grace)).
    pub shutdown_grace: Option<Duration>,
    /// What the network's server owns: who counts what is unread, who checks a delete (KT-2).
    pub server: ServerRules,
}

impl NetRules {
    /// The rules `descriptor` states.
    pub(crate) fn of(descriptor: &NetworkDescriptor) -> Self {
        Self {
            scope: descriptor.msg_id_scope,
            edit_target: descriptor.edit_target,
            outbox: descriptor.outbox,
            history_read: descriptor.history_read,
            shutdown_grace: descriptor.shutdown_grace,
            server: descriptor.server,
        }
    }

    /// Signal's rules with `scope`: the tests of the outbox that do not care about the rest.
    #[cfg(test)]
    pub(crate) fn signal(scope: MsgIdScope) -> Self {
        Self {
            scope,
            edit_target: EditTarget::LatestRevision,
            outbox: OutboxPolicy::SIGNAL,
            history_read: HistoryReadState::Known,
            shutdown_grace: None,
            server: ServerRules::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F1: the rules carry what a network says about the read flag of its history rows, and
    /// Signal's and the plain fake's are known.
    #[test]
    fn the_rules_carry_what_the_descriptor_says_of_its_history_read_flags() {
        let mut descriptor = kourier_fake::Flavor::SignalLike.descriptor();
        assert_eq!(
            NetRules::of(&descriptor).history_read,
            HistoryReadState::Known
        );
        descriptor.history_read = HistoryReadState::Unknown;
        assert_eq!(
            NetRules::of(&descriptor).history_read,
            HistoryReadState::Unknown
        );
    }

    /// R-Q1: the rules carry the stop grace a descriptor states, and none when it states none.
    #[test]
    fn the_rules_carry_the_stop_grace_the_descriptor_states() {
        let mut descriptor = kourier_fake::Flavor::SignalLike.descriptor();
        assert_eq!(NetRules::of(&descriptor).shutdown_grace, None);
        descriptor.shutdown_grace = Some(Duration::from_secs(20));
        assert_eq!(
            NetRules::of(&descriptor).shutdown_grace,
            Some(Duration::from_secs(20))
        );
    }

    /// KT-2: the rules carry what the descriptor says the server owns, and nothing on Signal-like
    /// networks.
    #[test]
    fn the_rules_carry_what_the_descriptor_says_the_server_owns() {
        let mut descriptor = kourier_fake::Flavor::SignalLike.descriptor();
        assert_eq!(NetRules::of(&descriptor).server, ServerRules::default());
        let server = ServerRules {
            owns_unread: true,
            validates_deletes: true,
        };
        descriptor.server = server;
        assert_eq!(NetRules::of(&descriptor).server, server);
    }
}
