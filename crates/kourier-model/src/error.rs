//! The backend error model (architecture §4.6) and the core's persist error.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ids::UserId;
use crate::log::Redacted;

/// An error a backend reports. `detail` must be built from log-safe values only (§9.1);
/// `user_message` is shown in the UI and may name people, so `Debug` hides it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind}: {detail}")]
pub struct BackendError {
    /// What went wrong, for the core's retry and pause rules.
    pub kind: ErrorKind,
    /// Log-safe detail.
    #[serde(default)]
    pub detail: String,
    /// User-facing text, when the backend has better words than the kind.
    #[serde(default)]
    pub user_message: Option<String>,
}

impl BackendError {
    /// An error of `kind` with a log-safe detail.
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            user_message: None,
        }
    }

    /// A transient error without a retry hint.
    pub fn transient(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Transient { retry_after: None }, detail)
    }

    /// The network or backend does not support the request.
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, detail)
    }

    /// A bug or broken invariant in the backend.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, detail)
    }

    /// Adds a user-facing message.
    pub fn with_user_message(mut self, message: impl Into<String>) -> Self {
        self.user_message = Some(message.into());
        self
    }

    /// Whether the same request may succeed later without user action.
    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }
}

impl fmt::Debug for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendError")
            .field("kind", &self.kind)
            .field("detail", &self.detail)
            .field(
                "user_message",
                &self.user_message.as_ref().map(|_| Redacted::new(())),
            )
            .finish()
    }
}

/// Error kinds (§4.6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    /// Network trouble, 5xx, timeouts.
    Transient {
        /// Server hint.
        #[serde(default)]
        retry_after: Option<Duration>,
    },
    /// 413/429.
    RateLimited {
        /// Server hint.
        #[serde(default)]
        retry_after: Option<Duration>,
    },
    /// The user must solve a challenge (Signal 428, captcha).
    Challenge {
        /// The challenge token (secret).
        token: Redacted<String>,
        /// Where the user solves it.
        url: String,
        /// When the server takes sends again without an answer (its Retry-After), when it
        /// said so. The core then lifts its pause by itself at that time and tries the
        /// waiting sends once (Signal Desktop's `scheduleRetry`,
        /// `SD/ts/challenge.dom.ts`); an accepted answer lifts it earlier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after: Option<Duration>,
    },
    /// The credentials were rejected (account-level).
    Unauthorized,
    /// The device was unlinked (account-level).
    LoggedOut,
    /// The recipient is not registered.
    PeerUnregistered,
    /// A safety number changed; needs `ConfirmIdentity`.
    UntrustedIdentity {
        /// Whose identity.
        user: UserId,
    },
    /// Too large for the network.
    TooLarge,
    /// The request was malformed.
    InvalidInput,
    /// Not supported by the network or backend.
    Unsupported,
    /// The remote content is gone (expired attachment, §7.1).
    Expired,
    /// A link was refused because the identity it brought is already an account of this
    /// profile (the same primary own id: Signal's ACI). `logged_out`: that account is logged
    /// out, so "Link Again" on it is the way back.
    AlreadyAdded {
        /// The existing account is logged out.
        #[serde(default)]
        logged_out: bool,
    },
    /// A bug.
    Internal,
}

impl ErrorKind {
    /// `Transient` and `RateLimited`.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient { .. } | Self::RateLimited { .. })
    }

    /// The server's retry hint (for a challenge: when it takes sends again unanswered).
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Transient { retry_after }
            | Self::RateLimited { retry_after }
            | Self::Challenge { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The variant name (log-safe).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Transient { .. } => "Transient",
            Self::RateLimited { .. } => "RateLimited",
            Self::Challenge { .. } => "Challenge",
            Self::Unauthorized => "Unauthorized",
            Self::LoggedOut => "LoggedOut",
            Self::PeerUnregistered => "PeerUnregistered",
            Self::UntrustedIdentity { .. } => "UntrustedIdentity",
            Self::TooLarge => "TooLarge",
            Self::InvalidInput => "InvalidInput",
            Self::Unsupported => "Unsupported",
            Self::Expired => "Expired",
            Self::AlreadyAdded { .. } => "AlreadyAdded",
            Self::Internal => "Internal",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The core could not persist an `Envelope`; the backend keeps its `core_outbox` rows and
/// forwards them again. `detail` is log-safe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("persist failed ({}): {detail}", if *.retryable { "retryable" } else { "fatal" })]
pub struct PersistError {
    /// Whether forwarding again later may succeed (busy, I/O) or not (corrupt data).
    pub retryable: bool,
    /// Log-safe detail.
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_debug_are_log_safe() {
        let e = BackendError::new(
            ErrorKind::Challenge {
                token: Redacted::new("tok-123".into()),
                url: "signalcaptcha://".into(),
                retry_after: Some(Duration::from_secs(90)),
            },
            "428 from server",
        )
        .with_user_message("Alice needs you to verify");
        let s = format!("{e} {e:?} {e:#?}");
        assert!(!s.contains("tok-123") && !s.contains("Alice"), "{s}");
        assert_eq!(e.to_string(), "Challenge: 428 from server");
        assert!(!e.is_retryable());
        assert_eq!(e.kind.retry_after(), Some(Duration::from_secs(90)));
        // A row stored before the field existed has no retry time, and none is written.
        let old: ErrorKind =
            serde_json::from_str(r#"{"Challenge":{"token":"t","url":"u"}}"#).expect("old row");
        assert_eq!(old.retry_after(), None);
        assert!(
            !serde_json::to_string(&old)
                .expect("serialize")
                .contains("retry_after")
        );
    }

    #[test]
    fn retry_hints() {
        let k = ErrorKind::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        };
        assert!(k.is_retryable());
        assert_eq!(k.retry_after(), Some(Duration::from_secs(3)));
        assert_eq!(ErrorKind::Internal.retry_after(), None);
        assert!(BackendError::transient("timeout").is_retryable());
        let p = PersistError {
            retryable: true,
            detail: "busy".into(),
        };
        assert_eq!(p.to_string(), "persist failed (retryable): busy");
    }
}
