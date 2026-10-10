//! The backend's internal error type and the log-safe view of libsignal errors (§4.6, §9.1).

use std::time::Duration;

use kourier_model::{BackendError, ErrorKind};
use libsignal_protocol::SignalProtocolError;

use crate::net::TransportError;

/// Why an operation inside the Signal backend failed. Every `Display` is log-safe: libsignal
/// errors are reduced to their variant name (some of them print service ids), SQLite errors
/// carry no bound values.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// `backend.db` failed.
    #[error("backend.db: {0}")]
    Db(#[from] kourier_db::Error),
    /// libsignal refused or failed.
    #[error("libsignal: {}", protocol_error_kind(.0))]
    Protocol(#[from] SignalProtocolError),
    /// The network failed.
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    /// A `core_outbox` row could not be encoded.
    #[error("core_outbox: {0}")]
    Outbox(#[from] kourier_backend_api::CodecError),
    /// Bytes from the network or the database did not decode (what, log-safe).
    #[error("could not decode {0}")]
    Decode(&'static str),
    /// The server answered with an unexpected status.
    #[error("server answered {status} to {what}")]
    Status {
        /// What was requested (log-safe route name).
        what: &'static str,
        /// The HTTP status.
        status: u16,
    },
    /// The server wants a challenge solved before more sends (428).
    #[error("the server asks for a challenge")]
    Challenge {
        /// The challenge token (secret; redacted in `Debug`).
        token: kourier_model::Redacted<String>,
        /// `Retry-After`: when the server takes such requests again without an answer.
        retry_after_secs: Option<u64>,
    },
    /// The server rate-limited a send (413/429).
    #[error("rate limited")]
    RateLimited {
        /// `Retry-After`, when the server sent one.
        retry_after_secs: Option<u64>,
    },
    /// The account is not linked (no `account` row).
    #[error("not linked")]
    NotLinked,
    /// The account was linked on another server than the one this start names
    /// (`net::env::check_binding`): nothing connects and nothing is deleted.
    #[error("this account is linked to {linked}, not to {now}; refusing to connect")]
    WrongEnvironment {
        /// The server the account is bound to (`account.env`; log-safe).
        linked: String,
        /// The server this start was told to use.
        now: String,
    },
    /// The core went away (its channel closed).
    #[error("the core stopped listening")]
    CoreGone,
    /// The core answered an envelope with a failed commit.
    #[error("the core did not commit (retryable: {retryable}): {detail}")]
    CoreRejected {
        /// The core may succeed later.
        retryable: bool,
        /// The core's log-safe reason.
        detail: String,
    },
    /// A broken invariant.
    #[error("internal: {0}")]
    Internal(&'static str),
}

impl From<kourier_db::rusqlite::Error> for Error {
    fn from(e: kourier_db::rusqlite::Error) -> Self {
        Self::Db(e.into())
    }
}

/// Where the user solves a send challenge (Desktop's `signalcaptcha://` flow starts here,
/// `SD/ts/challenge.dom.ts`).
pub(crate) const CHALLENGE_URL: &str = "https://signalcaptchas.org/challenge/generate.html";

/// `Result` with the backend's [`Error`].
pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// The contract form for the core (§4.6).
    pub(crate) fn to_backend_error(&self) -> BackendError {
        let detail = self.to_string();
        match self {
            Self::Transport(TransportError::RetryLater { retry_after_secs }) => BackendError::new(
                ErrorKind::RateLimited {
                    retry_after: Some(Duration::from_secs(u64::from(*retry_after_secs))),
                },
                detail,
            ),
            // A challenge the typed send API reported (the multi-recipient send): one the
            // user can answer when it offers a captcha, else a rate limit that lifts itself.
            Self::Transport(TransportError::Challenge {
                token,
                captcha: true,
                retry_after_secs,
            }) => BackendError::new(
                ErrorKind::Challenge {
                    token: token.clone(),
                    url: CHALLENGE_URL.to_owned(),
                    retry_after: retry_after_secs.map(|s| Duration::from_secs(u64::from(s))),
                },
                detail,
            ),
            Self::Transport(TransportError::Challenge {
                retry_after_secs, ..
            }) => BackendError::new(
                ErrorKind::RateLimited {
                    retry_after: retry_after_secs.map(|s| Duration::from_secs(u64::from(s))),
                },
                detail,
            ),
            Self::Transport(TransportError::DeviceDeregistered) => {
                BackendError::new(ErrorKind::LoggedOut, detail)
            }
            Self::Transport(TransportError::Rejected { status: 401 | 403 }) => {
                BackendError::new(ErrorKind::Unauthorized, detail)
            }
            Self::Transport(_) | Self::CoreGone | Self::CoreRejected { .. } => {
                BackendError::transient(detail)
            }
            Self::Challenge {
                token,
                retry_after_secs,
            } => BackendError::new(
                ErrorKind::Challenge {
                    token: token.clone(),
                    url: CHALLENGE_URL.to_owned(),
                    retry_after: retry_after_secs.map(Duration::from_secs),
                },
                detail,
            ),
            Self::RateLimited { retry_after_secs } => BackendError::new(
                ErrorKind::RateLimited {
                    retry_after: retry_after_secs.map(Duration::from_secs),
                },
                detail,
            ),
            Self::Status { status, .. } if *status >= 500 => BackendError::transient(detail),
            Self::Status {
                status: 401 | 403, ..
            } => BackendError::new(ErrorKind::Unauthorized, detail),
            Self::Status { status: 404, .. } => {
                BackendError::new(ErrorKind::PeerUnregistered, detail)
            }
            Self::Status {
                status: 413 | 429, ..
            } => BackendError::new(ErrorKind::RateLimited { retry_after: None }, detail),
            // No restart can help, and the core must not try one (it halts the account with
            // these words, §4.3 step 4).
            Self::WrongEnvironment { linked, now } => {
                BackendError::new(ErrorKind::Unsupported, detail).with_user_message(format!(
                    "This account was linked to {linked}, and Kourier was started for {now}. \
                     Nothing was changed. Start Kourier for {linked} again."
                ))
            }
            Self::Db(_)
            | Self::Protocol(_)
            | Self::Outbox(_)
            | Self::Decode(_)
            | Self::Status { .. }
            | Self::NotLinked
            | Self::Internal(_) => BackendError::internal(detail),
        }
    }
}

/// A send challenge the user can answer, as an error reports it.
pub(crate) struct ChallengeInfo<'a> {
    /// The token the answer must name (secret).
    pub(crate) token: &'a kourier_model::Redacted<String>,
    /// The server's `Retry-After`.
    pub(crate) retry_after: Option<Duration>,
}

impl Error {
    /// The challenge this error is, when the user can answer it (a 428 with a token and a
    /// captcha, from the REST send path or the typed one).
    pub(crate) fn challenge(&self) -> Option<ChallengeInfo<'_>> {
        match self {
            Self::Challenge {
                token,
                retry_after_secs,
            } => Some(ChallengeInfo {
                token,
                retry_after: retry_after_secs.map(Duration::from_secs),
            }),
            Self::Transport(TransportError::Challenge {
                token,
                captcha: true,
                retry_after_secs,
            }) => Some(ChallengeInfo {
                token,
                retry_after: retry_after_secs.map(|s| Duration::from_secs(u64::from(s))),
            }),
            _ => None,
        }
    }

    /// How long the server asked to wait before the request is made again (`Retry-After` of
    /// a rate limit or a challenge), when it said.
    pub(crate) fn server_wait(&self) -> Option<Duration> {
        match self {
            Self::Challenge {
                retry_after_secs, ..
            }
            | Self::RateLimited { retry_after_secs } => retry_after_secs.map(Duration::from_secs),
            Self::Transport(TransportError::Challenge {
                retry_after_secs, ..
            }) => retry_after_secs.map(|s| Duration::from_secs(u64::from(s))),
            Self::Transport(TransportError::RetryLater { retry_after_secs }) => {
                Some(Duration::from_secs(u64::from(*retry_after_secs)))
            }
            _ => None,
        }
    }
}

/// A log-safe name for a libsignal error. Its `Display` can print protocol addresses (service
/// ids) and sealed-sender details, so only the variant goes into logs and error details.
pub(crate) fn protocol_error_kind(e: &SignalProtocolError) -> &'static str {
    use SignalProtocolError as E;
    match e {
        E::InvalidArgument(_) => "invalid argument",
        E::InvalidState(op, _) => op,
        E::InvalidProtobufEncoding => "invalid protobuf encoding",
        E::CiphertextMessageTooShort(_) => "ciphertext too short",
        E::LegacyCiphertextVersion(_) => "legacy ciphertext version",
        E::UnrecognizedCiphertextVersion(_) => "unrecognized ciphertext version",
        E::UnrecognizedMessageVersion(_) => "unrecognized message version",
        E::NoKeyTypeIdentifier => "no key type identifier",
        E::BadKeyType(_) => "bad key type",
        E::BadKeyLength(..) => "bad key length",
        E::InvalidKeyAgreement => "invalid key agreement",
        E::SignatureValidationFailed => "signature validation failed",
        E::UntrustedIdentity(_) => "untrusted identity",
        E::InvalidPreKeyId => "invalid prekey id",
        E::InvalidSignedPreKeyId => "invalid signed prekey id",
        E::InvalidKyberPreKeyId => "invalid Kyber prekey id",
        E::InvalidMacKeyLength(_) => "invalid MAC key length",
        E::NoSenderKeyState { .. } => "no sender key state",
        E::InvalidProtocolAddress { .. } => "invalid protocol address",
        E::SessionNotFound(_) => "session not found",
        E::InvalidSessionStructure(_) => "invalid session structure",
        E::InvalidSenderKeySession { .. } => "invalid sender key session",
        E::InvalidRegistrationId(..) => "invalid registration id",
        E::DuplicatedMessage(..) => "duplicated message",
        E::InvalidMessage(..) => "invalid message",
        E::FfiBindingError(_) => "ffi binding error",
        E::ApplicationCallbackError(op, _) => op,
        E::InvalidSealedSenderMessage(_) => "invalid sealed sender message",
        E::UnknownSealedSenderVersion(_) => "unknown sealed sender version",
        E::SealedSenderSelfSend => "sealed sender self send",
        E::UnknownSealedSenderServerCertificateId(_) => "unknown server certificate id",
        E::BadKEMKeyType(_) => "bad KEM key type",
        E::WrongKEMKeyType(..) => "wrong KEM key type",
        E::BadKEMKeyLength(..) => "bad KEM key length",
        E::BadKEMCiphertextLength(..) => "bad KEM ciphertext length",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsignal_protocol::{DeviceId, ProtocolAddress};

    #[test]
    fn protocol_errors_never_print_addresses() {
        let address = ProtocolAddress::new(
            "9d0652a3-dcc3-4d11-975f-74d61598733f".to_owned(),
            DeviceId::new(1).unwrap(),
        );
        let e = Error::from(SignalProtocolError::UntrustedIdentity(address));
        let text = e.to_string();
        assert_eq!(text, "libsignal: untrusted identity");
        assert!(!text.contains("9d06"));
    }

    /// A challenge reaches the core with the time the server gave, whichever send path met
    /// it; one without a captcha is a rate limit.
    #[test]
    fn challenges_carry_their_retry_time() {
        let token = || kourier_model::Redacted::new("t0k".to_owned());
        let rest = Error::Challenge {
            token: token(),
            retry_after_secs: Some(90),
        };
        let typed = Error::from(TransportError::Challenge {
            token: token(),
            captcha: true,
            retry_after_secs: Some(90),
        });
        for e in [&rest, &typed] {
            let ErrorKind::Challenge {
                token: t,
                url,
                retry_after,
            } = e.to_backend_error().kind
            else {
                panic!("{e:?}");
            };
            assert_eq!(t.expose(), "t0k");
            assert_eq!(url, CHALLENGE_URL);
            assert_eq!(retry_after, Some(Duration::from_secs(90)));
            let info = e.challenge().expect("a challenge");
            assert_eq!(info.token.expose(), "t0k");
            assert_eq!(info.retry_after, Some(Duration::from_secs(90)));
            assert_eq!(e.server_wait(), Some(Duration::from_secs(90)));
            assert!(
                !format!("{e:?} {e}").contains("t0k"),
                "the token is never printed"
            );
        }
        // Nothing a person here can solve: wait it out.
        let unsolvable = Error::from(TransportError::Challenge {
            token: token(),
            captcha: false,
            retry_after_secs: Some(30),
        });
        assert!(unsolvable.challenge().is_none());
        assert_eq!(
            unsolvable.to_backend_error().kind,
            ErrorKind::RateLimited {
                retry_after: Some(Duration::from_secs(30))
            }
        );
        let none = Error::Challenge {
            token: token(),
            retry_after_secs: None,
        };
        assert_eq!(none.server_wait(), None);
        assert!(matches!(
            none.to_backend_error().kind,
            ErrorKind::Challenge {
                retry_after: None,
                ..
            }
        ));
    }

    #[test]
    fn statuses_map_to_contract_kinds() {
        let status = |status| Error::Status {
            what: "messages",
            status,
        };
        assert_eq!(
            status(404).to_backend_error().kind,
            ErrorKind::PeerUnregistered
        );
        assert!(status(503).to_backend_error().is_retryable());
        assert_eq!(status(403).to_backend_error().kind, ErrorKind::Unauthorized);
        assert_eq!(status(400).to_backend_error().kind, ErrorKind::Internal);
    }
}
