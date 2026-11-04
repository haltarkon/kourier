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
    /// The network refuses this account a connection for now (a WhatsApp temporary ban). The
    /// account stays as it is; the core holds the outbox while this lasts and the backend
    /// leaves the state when `until_ms` has passed (or the server lets it in again). A
    /// restriction that leaves the connection up (WhatsApp's reach-out time lock) is NOT this
    /// state but an [`AccountHold`].
    Restricted {
        /// Until (ms), when known.
        #[serde(default)]
        until_ms: Option<u64>,
        /// Log-safe detail for the log. The UI never shows it: it writes its own translated
        /// text from `kind`.
        reason: String,
        /// What kind of restriction it is, typed so the UI can say why (R4: the words are the
        /// UI's). `Unknown` for a backend that says no more.
        #[serde(default)]
        kind: Restriction,
        /// What the network said itself (its own text, a page to appeal on).
        #[serde(default)]
        note: NetworkNote,
    },
    /// Unlinked.
    LoggedOut {
        /// Log-safe reason.
        reason: String,
        /// Why, when the network said (the UI writes a sentence for it); `Unknown` keeps the
        /// generic "no longer linked".
        #[serde(default)]
        cause: LogoutCause,
        /// What the network said itself (its own text, a page to appeal on).
        #[serde(default)]
        note: NetworkNote,
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

/// What the network itself said about a ban or a logout, to be shown after Kourier's own
/// sentence (the person asked for "their text as well"). Both parts come from the network, so
/// the UI treats them as untrusted: the explanation is plain text only (never markup, never
/// logged), the page is opened only as an `https://` address and only after a click
/// (architecture §9.4). `Debug` says only which parts are there.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkNote {
    /// The network's own words (WhatsApp's explanation of a ban or a logout).
    #[serde(default)]
    pub explanation: Option<String>,
    /// A page to appeal or to learn more (WhatsApp's support page).
    #[serde(default)]
    pub appeal_url: Option<String>,
}

impl std::fmt::Debug for NetworkNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkNote")
            .field("explanation", &self.explanation.as_ref().map(|_| "…"))
            .field("appeal_url", &self.appeal_url.as_ref().map(|_| "…"))
            .finish()
    }
}

impl NetworkNote {
    /// Nothing was said.
    pub fn is_empty(&self) -> bool {
        self.explanation.is_none() && self.appeal_url.is_none()
    }
}

/// What kind of [`ConnState::Restricted`] it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Restriction {
    /// The backend could say no more.
    #[default]
    Unknown,
    /// A temporary ban: no connection until `until_ms` (WhatsApp's 402 at connect).
    Banned(BanReason),
}

/// Why an account was banned for a while, mapped by the backend from the network's code
/// (WhatsApp's `TempBanReason`). The UI writes one sentence for each; the codes are not
/// shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BanReason {
    /// Messages to too many people who have not saved the account's number (WhatsApp 101).
    TooManyStrangers,
    /// Too many people blocked the account (102).
    ManyBlocked,
    /// Too many groups created with people who have not saved the number (103).
    TooManyGroups,
    /// The same message sent to too many people (104).
    SameMessageToMany,
    /// Too many messages to a broadcast list (106).
    BroadcastList,
    /// Any other code (only logged; the UI says "banned" without a reason).
    Other(i32),
}

/// Why an account was logged out, when the network said. The raw detail stays in
/// [`ConnState::LoggedOut::reason`] for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LogoutCause {
    /// Not said (every Signal logout today).
    #[default]
    Unknown,
    /// The device was removed from the list of linked devices on the phone.
    RemovedOnPhone,
    /// The phone was not used for too long and the network dropped the linked device.
    PhoneInactive,
    /// The network closed or locked the account itself.
    AccountGone,
    /// The network refused the account because it was using an unofficial application.
    Unofficial,
}

/// A temporary hold on what the user sends from one account while the connection stays up
/// (WhatsApp's reach-out time lock; architecture §4.5). Messages, reactions, edits and
/// deletes made in Kourier wait until `until_ms` (the core keeps them in the outbox) and a
/// change of a conversation's disappearing-message timer is refused meanwhile; receiving,
/// downloads, read receipts and the missed-call summary go on. The backend states it with
/// `BackendEvent::AccountHold(Some(..))` when the network tells (and again at every connect,
/// before its first `Syncing` or `Online`), and lifts it with `AccountHold(None)`; the core
/// keeps it across a restart of Kourier and also lets it go by itself at `until_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountHold {
    /// Until (ms), when the network said; without it the hold lasts until the backend lifts it.
    #[serde(default)]
    pub until_ms: Option<u64>,
    /// Why.
    #[serde(default)]
    pub reason: HoldReason,
}

/// Why an [`AccountHold`] is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HoldReason {
    /// The backend could say no more.
    #[default]
    Unknown,
    /// Messages to people who have not written to the account are refused until the time
    /// lock ends (WhatsApp).
    ReachOutLocked,
    /// The network froze the account (Telegram): it can read, what it sends is refused, and it
    /// is deleted at a date unless the owner appeals (in the network's own app). Lifted when the
    /// network says so; no `until_ms`.
    Frozen,
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
