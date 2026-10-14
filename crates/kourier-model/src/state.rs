//! Outgoing and incoming message states, receipts and per-recipient send state (§3.7).

use serde::{Deserialize, Serialize};

use crate::error::ErrorKind;

/// Aggregated state of an own message. Monotonic along [`OutgoingState::rank`]; only a user
/// "Retry" moves a failed message back to `Queued`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OutgoingState {
    /// In the core outbox.
    Queued,
    /// Being transmitted.
    Sending {
        /// Upload progress 0.0 … 1.0, when known.
        #[serde(default)]
        progress: Option<f32>,
    },
    /// Accepted by the server.
    Sent {
        /// Server timestamp.
        #[serde(default)]
        server_at: Option<i64>,
    },
    /// Delivered to every recipient.
    Delivered,
    /// Read by every recipient.
    Read,
    /// Viewed (media) by every recipient.
    Viewed,
    /// Not sent.
    Failed {
        /// Why.
        reason: FailReason,
        /// Whether "Retry" may help.
        retryable: bool,
    },
    /// Groups: some recipients failed for good.
    PartiallyFailed {
        /// Who failed.
        failed: Vec<crate::ids::UserId>,
    },
}

impl OutgoingState {
    /// Position in the forward order `Queued < Sending < Sent < Delivered < Read < Viewed`
    /// (Signal's `MessageSendState`); `None` for the failure states.
    pub fn rank(&self) -> Option<u8> {
        match self {
            Self::Queued => Some(0),
            Self::Sending { .. } => Some(1),
            Self::Sent { .. } => Some(2),
            Self::Delivered => Some(3),
            Self::Read => Some(4),
            Self::Viewed => Some(5),
            Self::Failed { .. } | Self::PartiallyFailed { .. } => None,
        }
    }
}

/// Why a message failed, as stored with [`OutgoingState::Failed`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailReason {
    /// Network trouble; the retry budget ran out.
    Network,
    /// Rate limited for too long.
    RateLimited,
    /// The network wants a challenge solved.
    Challenge,
    /// The recipient is not registered any more.
    Unregistered,
    /// A safety number changed and was not confirmed.
    UntrustedIdentity,
    /// Too large for the network.
    TooLarge,
    /// The network does not support it here.
    Unsupported,
    /// The account was logged out.
    LoggedOut,
    /// Anything else; log-safe detail.
    Other(String),
}

impl From<&ErrorKind> for FailReason {
    fn from(kind: &ErrorKind) -> Self {
        match kind {
            ErrorKind::Transient { .. } => Self::Network,
            ErrorKind::RateLimited { .. } => Self::RateLimited,
            ErrorKind::Challenge { .. } => Self::Challenge,
            ErrorKind::Unauthorized | ErrorKind::LoggedOut => Self::LoggedOut,
            ErrorKind::PeerUnregistered => Self::Unregistered,
            ErrorKind::UntrustedIdentity { .. } => Self::UntrustedIdentity,
            ErrorKind::TooLarge => Self::TooLarge,
            ErrorKind::Unsupported => Self::Unsupported,
            ErrorKind::InvalidInput => Self::Other("invalid input".into()),
            ErrorKind::Expired => Self::Other("expired".into()),
            ErrorKind::AlreadyAdded { .. } => Self::Other("already added".into()),
            ErrorKind::Internal => Self::Other("internal error".into()),
        }
    }
}

/// Read state of an incoming message. Ordered: only moves forward.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum ReadState {
    /// Not read yet.
    #[default]
    Unread,
    /// Read.
    Read,
    /// Media viewed.
    Viewed,
}

/// A receipt kind. Ordered like the states it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReceiptKind {
    /// Delivered to a device of the recipient.
    Delivered,
    /// Read.
    Read,
    /// Media viewed.
    Viewed,
}

/// Send outcome for one recipient (the core's `send_state` row; receipts are separate).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecipientState {
    /// Not sent yet.
    Pending,
    /// Accepted by the server for this recipient.
    Sent {
        /// Sent without revealing the sender to the server (Signal sealed sender).
        #[serde(default)]
        sealed: bool,
    },
    /// Failed for this recipient.
    Failed {
        /// Whether a resend to this recipient may help.
        retryable: bool,
    },
}

impl RecipientState {
    /// Whether a retry must include this recipient (`only` in a resend, §4.8).
    pub fn needs_send(&self) -> bool {
        !matches!(self, Self::Sent { .. })
    }
}

/// What a running send of an own message is doing (link feedback L6): ephemeral, reported by
/// the backend while an attempt runs and never stored. A restart starts the attempt over and
/// the phases come again. Counts only, log-safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendPhase {
    /// Uploading the attachments (the share is `TransmitProgress::progress`).
    Uploading,
    /// Fetching the group's current state before sending.
    RefreshingGroup,
    /// Setting up encryption with members that have no keys from us yet: `done` of `of`
    /// members (key fetches, then the sender key handed out).
    SettingUpKeys {
        /// Members done.
        done: u32,
        /// Members that need it.
        of: u32,
    },
    /// The server named devices that changed: updating them and sending again.
    RepairingDevices,
    /// Sending to the members: `done` of `of` the server took it for.
    Sending {
        /// Members the server took it for.
        done: u32,
        /// Members of this send.
        of: u32,
    },
    /// Sent to the members; telling our other devices.
    SyncingOwnDevices,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_orders() {
        let order = [
            OutgoingState::Queued,
            OutgoingState::Sending { progress: None },
            OutgoingState::Sent { server_at: None },
            OutgoingState::Delivered,
            OutgoingState::Read,
            OutgoingState::Viewed,
        ];
        let ranks: Vec<_> = order.iter().map(OutgoingState::rank).collect();
        assert!(ranks.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(
            OutgoingState::PartiallyFailed { failed: vec![] }.rank(),
            None
        );
        assert!(ReadState::Unread < ReadState::Read && ReadState::Read < ReadState::Viewed);
        assert!(ReceiptKind::Delivered < ReceiptKind::Read);
        assert!(RecipientState::Pending.needs_send());
        assert!(!RecipientState::Sent { sealed: true }.needs_send());
    }
}
