//! Connection state of an account (architecture §4.5).

use serde::{Deserialize, Serialize};

/// Connection state, published by the backend on a `watch` channel.
///
/// `Syncing` lasts until every envelope up to the catch-up boundary has passed the backend's
/// stage 2 *and* been acked by the core; messages emitted meanwhile carry `catch_up = true`.
/// The core drains its outbox as soon as the connection is up ([`ConnState::can_send`]: `Syncing`
/// or `Online`, as Signal Desktop, link feedback L6); the backend's side-effect queues wait for
/// `Online`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConnState {
    /// Not connected.
    Offline {
        /// Next reconnect attempt (ms), when scheduled.
        #[serde(default)]
        retry_at_ms: Option<u64>,
    },
    /// Connecting.
    Connecting,
    /// Catching up on the server backlog.
    Syncing {
        /// Progress 0.0 … 1.0, when known.
        #[serde(default)]
        fraction: Option<f32>,
    },
    /// Connected and caught up.
    Online {
        /// Network-specific detail (Threema "leader").
        #[serde(default)]
        detail: Option<String>,
    },
    /// The user must act.
    NeedsUserAction(UserAction),
    /// Temporarily restricted (WhatsApp ban, reach-out lock).
    Restricted {
        /// Until (ms), when known.
        #[serde(default)]
        until_ms: Option<u64>,
        /// User-facing reason.
        reason: String,
    },
    /// Unlinked.
    LoggedOut {
        /// Log-safe reason.
        reason: String,
    },
}

impl Default for ConnState {
    fn default() -> Self {
        Self::Offline { retry_at_ms: None }
    }
}

impl ConnState {
    /// Connected and caught up.
    pub fn is_online(&self) -> bool {
        matches!(self, Self::Online { .. })
    }

    /// The connection is up (catching up or caught up): the outbox may drain. Signal Desktop
    /// sends while it catches up too (its job queues wait only for the socket).
    pub fn can_send(&self) -> bool {
        matches!(self, Self::Syncing { .. } | Self::Online { .. })
    }

    /// The variant name (log-safe).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Offline { .. } => "Offline",
            Self::Connecting => "Connecting",
            Self::Syncing { .. } => "Syncing",
            Self::Online { .. } => "Online",
            Self::NeedsUserAction(_) => "NeedsUserAction",
            Self::Restricted { .. } => "Restricted",
            Self::LoggedOut { .. } => "LoggedOut",
        }
    }
}

/// What the user must do.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UserAction {
    /// Link the device again.
    Relink,
    /// Update Kourier.
    UpdateApp,
    /// Solve a challenge (captcha) at this URL.
    SolveChallenge {
        /// Where.
        url: String,
    },
    /// Fix the system clock.
    FixClock {
        /// Measured skew (ms, positive = local clock ahead).
        skew_ms: i64,
    },
    /// Reconnect explicitly (another session took over).
    Reconnect,
}
