//! ONE table for why a send failed (W5a, decision D6; brief §2.3): the server's nack codes and the failures
//! before the wire ([`SendFailure`]) become the contract's [`BackendError`] here and nowhere else.
//!
//! - `463` -> [`ErrorKind::ReachOutRefused`] (the account may not reach this person first). NEVER retried by
//!   the backend, and the core never retries it either. So is `475` (upstream's `NewChatMessagesCapped`, a cap on
//!   messages to new chats) until a real run shows what it means on our acks (fix F8).
//! - `429` -> `RateLimited`; `500`, `503` and `421` (the group's addressing was stale) -> `Transient`. Each carries
//!   its FLOOR as `retry_after` (fix F1; [`Nack::floor`]: 429 five minutes, the others thirty seconds), the time
//!   the registry keeps the local from being written again on the connection; a remembered nack answers with what
//!   is left of it ([`nack_error`]). Upstream reads nothing of an ack's 421 (it only logs it): the first frame after
//!   its floor asks for fresh group metadata itself (`Outgoing::groups`).
//! - `404` -> `PeerUnregistered`; `400`, `479` (the server refused the stanza's shape) -> `InvalidInput`;
//!   `406`, `415` and every other or unreadable code -> `Unsupported`. Not retryable; Send Again may still go
//!   (nothing of these is recorded, critic H2).
//! - Before the wire: no client, not connected, not logged in yet (critic H4), a socket that failed, an IQ
//!   that timed out -> `Transient`; an IQ refused -> its status by the same codes; no device of the recipient
//!   (after the one retry with a fresh list, [`refresh_first`]) -> `PeerUnregistered`; a request upstream
//!   refuses -> `InvalidInput`; anything else -> `Internal`.
//!
//! - An UPLOAD (W5b, brief §2.6, [`upload_error`]): no client, nothing finished (the `w:m` IQ, the connection, the
//!   client, a read of the source), a 401/403 after upstream's one refetch of the `media_conn` (the next attempt
//!   asks again), 408 and every 5xx -> `Transient`, a 429 -> `RateLimited`; 413 and every other status ->
//!   `Unsupported` with the status in the detail (not retryable; Send Again may still go). FLOORS (fix F2): a retry is a
//!   whole new upload, so the retryable kinds carry `retry_after`: 429 five minutes ([`RATE_LIMITED_FLOOR`]), the rest
//!   thirty seconds ([`STALL_RETRY`]); `NoClient` none. A 3xx or a cut 2xx answer is `Incomplete` (no status is kept). A STOP is not read here: the attempt answers
//!   it from its own stop state (`stop::stopped`), whatever the upload's error says.
//!
//! The detail names the code and nothing else: no number, no JID, no id, no URL (it reaches logs).

use std::time::Duration;

use kourier_model::{BackendError, ErrorKind};

use super::acks::{Nack, RATE_LIMITED_FLOOR};
use super::upload::STALL_RETRY;
use crate::session::{SendFailure, UploadFailure};

/// Why one send failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    /// The server refused the frame.
    Nack(Nack),
    /// The frame was not written (or may have left on a socket that failed).
    Failure(SendFailure),
}

/// The contract's error of `cause`. A nack carries its whole floor as `retry_after` (see [`nack_error`] for a
/// remembered one).
pub(crate) fn error_of(cause: Cause) -> BackendError {
    match cause {
        Cause::Nack(nack) => nack_error(nack, nack.floor()),
        Cause::Failure(failure) => match failure {
            SendFailure::NoClient => BackendError::transient("no client runs"),
            SendFailure::NotConnected => BackendError::transient("not connected"),
            SendFailure::NotLoggedIn => {
                BackendError::transient("the account's own ids are not known yet")
            }
            SendFailure::Socket => BackendError::transient("the connection failed while sending"),
            SendFailure::IqTimeout => {
                BackendError::transient("a request of the send got no answer in time")
            }
            SendFailure::IqRefused { code } => {
                by_code(code, "a request of the send was refused", None)
            }
            SendFailure::NoRecipientDevice | SendFailure::PrimaryDeviceRejected => {
                BackendError::new(
                    ErrorKind::PeerUnregistered,
                    "no device of the recipient could be reached",
                )
            }
            SendFailure::InvalidRequest => BackendError::new(
                ErrorKind::InvalidInput,
                "the request was refused before sending",
            ),
            SendFailure::Internal => BackendError::internal("the send failed inside the client"),
        },
    }
}

/// The error of a server's nack whose retryable kinds say `retry_after` (the whole floor for a live one, what is
/// left of it for a remembered one: fix F1).
pub(crate) fn nack_error(nack: Nack, retry_after: Option<Duration>) -> BackendError {
    match nack {
        Nack::Code(code) => by_code(code, "the server refused the message", retry_after),
        Nack::Unreadable => BackendError::new(
            ErrorKind::Unsupported,
            "the server refused the message (an unreadable code)",
        ),
    }
}

/// The kind of a server code, with the code in the detail. `retry_after` is for the retryable kinds only.
fn by_code(code: u16, what: &str, retry_after: Option<Duration>) -> BackendError {
    let kind = match code {
        463 | 475 => ErrorKind::ReachOutRefused,
        429 => ErrorKind::RateLimited { retry_after },
        421 | 500 | 503 => ErrorKind::Transient { retry_after },
        404 => ErrorKind::PeerUnregistered,
        400 | 479 => ErrorKind::InvalidInput,
        _ => ErrorKind::Unsupported,
    };
    BackendError::new(kind, format!("{what} (code {code})"))
}

/// The contract's error of a failed upload (the module doc's table). The retryable kinds carry a FLOOR (fix F2, privacy
/// MEDIUM): each retry is a whole new upload under a fresh key (and after a 401 a `w:m` IQ on the chat's socket), so the
/// core must not repeat one at its own 0 ms, 190 ms, ... curve. A 429 waits [`RATE_LIMITED_FLOOR`] (W5a's number for the
/// chat server's 429), every other retryable failure [`STALL_RETRY`]; only `NoClient` has none (no request left the PC).
/// A 3xx or a cut 2xx answer reaches here as `Incomplete` too (no status is kept for them): a known limit.
pub(crate) fn upload_error(failure: UploadFailure) -> BackendError {
    let after = |detail: String| {
        BackendError::new(
            ErrorKind::Transient {
                retry_after: Some(STALL_RETRY),
            },
            detail,
        )
    };
    match failure {
        UploadFailure::NoClient => BackendError::transient("no client runs"),
        UploadFailure::Incomplete => after("the upload did not complete".to_owned()),
        UploadFailure::Status(code @ (401 | 403)) => after(format!(
            "the media server refused the upload's authorisation (code {code})"
        )),
        UploadFailure::Status(429) => BackendError::new(
            ErrorKind::RateLimited {
                retry_after: Some(RATE_LIMITED_FLOOR),
            },
            "the media server limited the upload (code 429)",
        ),
        UploadFailure::Status(code @ (408 | 500..=599)) => {
            after(format!("the media server failed the upload (code {code})"))
        }
        UploadFailure::Status(code) => BackendError::new(
            ErrorKind::Unsupported,
            format!("the media server refused the file (code {code})"),
        ),
    }
}

/// Whether a failure before the wire is tried ONCE more inside the attempt, with the recipient's device list
/// asked again (`Devices::Refresh`; brief §2.3): only when no device of the recipient could be used. Nothing
/// left on the wire, so the retry is no second frame.
pub(crate) fn refresh_first(failure: SendFailure) -> bool {
    matches!(
        failure,
        SendFailure::NoRecipientDevice | SendFailure::PrimaryDeviceRejected
    )
}

#[cfg(test)]
mod tests;
