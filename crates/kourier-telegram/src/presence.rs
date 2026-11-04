//! The account's online status and the core's network hints (TG-2 C8c; critic H6, M8; lead
//! T-3): pure rules on the actor's clock, carried out by the actor as requests of their own (never
//! the login's one pending request).
//!
//! - **Online** (`setOption online`, TDLib's `OnlineManager`): the account shows online only while
//!   a Kourier window has focus AND this account's conversation is open (his answer "Only the open
//!   account", T-3: the core fills [`UiPresence::visible`] only for the account whose chat is open,
//!   `kourier-core/src/supervisor.rs` `push_presence`, so no shared commit is needed) AND the session
//!   is not idle (always false until the core's B8 step). H6: `true` at once, `false` only after
//!   [`OFF_AFTER_MS`] without it, at most one change per [`MIN_GAP_MS`]; never before TDLib said
//!   `Ready` on a linked session; `false` before a helper's stop (the actor, best effort). TDLib's
//!   default is offline, so nothing is sent until the account is to show online.
//! - **Network hints** (`setNetworkType networkTypeOther`): the type bumps TDLib's network
//!   generation, which clears every back-off and flood control (M8), so `NetworkChanged` is passed
//!   on only while the last connection state is not `Ready`; `Resumed` always; at most once per
//!   [`HINT_GAP_MS`], and only while a helper runs. Never `networkTypeNone`.
//!
//! Nothing here logs; the requests carry no person's data.

use kourier_backend_api::UiPresence;
use serde_json::{Value, json};

use crate::tdapi::ConnectionState;

/// How long without focus (or without the open conversation) before TDLib is told offline (H6).
pub(crate) const OFF_AFTER_MS: u64 = 5_000;
/// The least time between two changes of the online status (H6: focus flapping).
pub(crate) const MIN_GAP_MS: u64 = 2_000;
/// The least time between two network types sent (M8).
pub(crate) const HINT_GAP_MS: u64 = 10_000;

/// A network hint of the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hint {
    /// `Command::NetworkChanged`.
    NetworkChanged,
    /// `Command::Resumed`.
    Resumed,
}

/// Whether the UI asks this account to show online (T-3).
pub(crate) fn online_of(ui: &UiPresence) -> bool {
    ui.focused && ui.visible.is_some() && !ui.idle
}

/// `setOption("online", optionValueBoolean)`.
pub(crate) fn online_request(online: bool) -> Value {
    json!({
        "@type": "setOption",
        "name": "online",
        "value": {"@type": "optionValueBoolean", "value": online},
    })
}

/// `setNetworkType(networkTypeOther)`: "something changed", the only type Kourier knows.
pub(crate) fn network_type_request() -> Value {
    json!({
        "@type": "setNetworkType",
        "type": {"@type": "networkTypeOther"},
    })
}

/// The online status's debounce (H6). Times are the actor's clock's milliseconds.
#[derive(Debug, Default)]
pub(crate) struct Presence {
    /// What the UI asks for now.
    want: bool,
    /// When `want` turned false (the start of the [`OFF_AFTER_MS`] wait).
    off_since: Option<u64>,
    /// TDLib said `Ready` on a linked session of the live helper.
    ready: bool,
    /// What this helper's TDLib was told last (`None`: nothing, its default offline).
    told: Option<bool>,
    /// When a change was told last (any helper): the [`MIN_GAP_MS`] rule.
    told_at: Option<u64>,
}

impl Presence {
    /// The UI's wish changed (or was read again).
    pub(crate) fn on_ui(&mut self, online: bool, now: u64) {
        if online == self.want {
            return;
        }
        self.want = online;
        self.off_since = if online { None } else { Some(now) };
    }

    /// Whether the account may be told anything now (a linked session past `Ready` on a live
    /// helper). It changes nothing that was told.
    pub(crate) fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    /// What TDLib shows now.
    fn current(&self) -> bool {
        self.told.unwrap_or(false)
    }

    /// The status to tell now, without the gap rule.
    fn target(&self, now: u64) -> Option<bool> {
        if !self.ready {
            return None;
        }
        match (self.want, self.current()) {
            (true, false) => Some(true),
            (false, true) => {
                let since = self.off_since.unwrap_or(now);
                (now >= since.saturating_add(OFF_AFTER_MS)).then_some(false)
            }
            _ => None,
        }
    }

    /// The status to tell TDLib now, if any (it counts as told).
    pub(crate) fn poll(&mut self, now: u64) -> Option<bool> {
        let value = self.target(now)?;
        if self
            .told_at
            .is_some_and(|at| now < at.saturating_add(MIN_GAP_MS))
        {
            return None;
        }
        self.told = Some(value);
        self.told_at = Some(now);
        Some(value)
    }

    /// When [`Presence::poll`] may have something to tell (always later than a `poll` at `now`
    /// that returned `None`; `None`: nothing waits).
    pub(crate) fn due_ms(&self) -> Option<u64> {
        if !self.ready || self.want == self.current() {
            return None;
        }
        let base = if self.want {
            0
        } else {
            self.off_since.unwrap_or(0).saturating_add(OFF_AFTER_MS)
        };
        let gap = self.told_at.map_or(0, |at| at.saturating_add(MIN_GAP_MS));
        Some(base.max(gap))
    }

    /// The helper stops: `true` when TDLib shows online and is to be told `false` first (best
    /// effort). Afterwards nothing is told until the next helper's `Ready`.
    pub(crate) fn on_stop(&mut self, now: u64) -> bool {
        let online = self.told == Some(true);
        if online {
            self.told_at = Some(now);
        }
        self.told = None;
        self.ready = false;
        online
    }
}

/// The network hints' rule (M8).
#[derive(Debug, Default)]
pub(crate) struct Hints {
    /// When a network type was sent last.
    sent_at: Option<u64>,
}

impl Hints {
    /// Whether this hint is passed on to TDLib (the caller checks that a helper runs first).
    pub(crate) fn on_hint(
        &mut self,
        hint: Hint,
        last_conn: Option<ConnectionState>,
        now: u64,
    ) -> bool {
        if hint == Hint::NetworkChanged && last_conn == Some(ConnectionState::Ready) {
            return false;
        }
        if self
            .sent_at
            .is_some_and(|at| now < at.saturating_add(HINT_GAP_MS))
        {
            return false;
        }
        self.sent_at = Some(now);
        true
    }
}

#[cfg(test)]
mod tests;
