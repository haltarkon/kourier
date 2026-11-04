//! The one error table (W5a, D6): every nack code and every failure before the wire has its kind, only 463 and 475 are
//! `ReachOutRefused`, the detail names the code and nothing else, and only a missing device is tried again.

use std::time::Duration;

use kourier_model::ErrorKind;

use super::{Cause, error_of, nack_error, refresh_first, upload_error};
use crate::send::acks::Nack;
use crate::session::{SendFailure, UploadFailure};

fn nack(code: u16) -> ErrorKind {
    error_of(Cause::Nack(Nack::Code(code))).kind
}

#[test]
fn every_nack_code_has_its_kind_and_only_463_is_a_refused_reach_out() {
    // 475 (a cap on messages to new chats) is met like a 463 until a real run shows what it means (fix F8).
    for code in [463, 475] {
        assert_eq!(nack(code), ErrorKind::ReachOutRefused, "code {code}");
        assert!(!error_of(Cause::Nack(Nack::Code(code))).is_retryable());
    }
    // The retryable kinds carry their FLOOR (fix F1): the core does not retry before it.
    assert_eq!(
        nack(429),
        ErrorKind::RateLimited {
            retry_after: Some(Duration::from_secs(300))
        }
    );
    for code in [421, 500, 503] {
        assert_eq!(
            nack(code),
            ErrorKind::Transient {
                retry_after: Some(Duration::from_secs(30))
            },
            "a {code} nack is tried again later, not before its floor"
        );
    }
    assert_eq!(nack(404), ErrorKind::PeerUnregistered);
    for code in [400, 479] {
        assert_eq!(nack(code), ErrorKind::InvalidInput, "code {code}");
    }
    for code in [406, 415, 999] {
        assert_eq!(nack(code), ErrorKind::Unsupported, "code {code}");
    }
    assert_eq!(
        error_of(Cause::Nack(Nack::Unreadable)).kind,
        ErrorKind::Unsupported
    );
    for code in [400, 404, 406, 415, 479, 999] {
        let kind = nack(code);
        assert!(
            !kind.is_retryable() && kind != ErrorKind::ReachOutRefused,
            "a {code} nack fails without a retry and is no refused reach-out"
        );
    }
    let detail = error_of(Cause::Nack(Nack::Code(479))).detail;
    assert!(
        detail.contains("479"),
        "the detail must name the code: {detail}"
    );
}

/// Fix F1: a remembered nack says what is left of its floor, a nack without a floor says nothing; a failure before
/// the wire (an IQ refused with a 429) has no floor (no frame left).
#[test]
fn a_remembered_nack_says_what_is_left_of_its_floor() {
    let left = Some(Duration::from_secs(41));
    assert_eq!(
        nack_error(Nack::Code(429), left).kind,
        ErrorKind::RateLimited { retry_after: left }
    );
    assert_eq!(
        nack_error(Nack::Code(500), left).kind,
        ErrorKind::Transient { retry_after: left }
    );
    assert_eq!(
        nack_error(Nack::Code(400), None).kind,
        ErrorKind::InvalidInput
    );
    assert_eq!(
        nack_error(Nack::Code(463), None).kind,
        ErrorKind::ReachOutRefused
    );
}

#[test]
fn a_failure_before_the_wire_is_read_by_the_same_table() {
    use SendFailure::*;
    for failure in [NoClient, NotConnected, NotLoggedIn, Socket, IqTimeout] {
        assert_eq!(
            error_of(Cause::Failure(failure)).kind,
            ErrorKind::Transient { retry_after: None },
            "{failure:?} is tried again later"
        );
    }
    assert_eq!(
        error_of(Cause::Failure(IqRefused { code: 429 })).kind,
        ErrorKind::RateLimited { retry_after: None }
    );
    assert_eq!(
        error_of(Cause::Failure(IqRefused { code: 463 })).kind,
        ErrorKind::ReachOutRefused
    );
    for failure in [NoRecipientDevice, PrimaryDeviceRejected] {
        assert_eq!(
            error_of(Cause::Failure(failure)).kind,
            ErrorKind::PeerUnregistered
        );
    }
    assert_eq!(
        error_of(Cause::Failure(InvalidRequest)).kind,
        ErrorKind::InvalidInput
    );
    assert_eq!(error_of(Cause::Failure(Internal)).kind, ErrorKind::Internal);
}

#[test]
fn only_a_recipient_without_a_usable_device_is_tried_again_with_a_fresh_list() {
    use SendFailure::*;
    assert!(refresh_first(NoRecipientDevice) && refresh_first(PrimaryDeviceRejected));
    for failure in [
        NoClient,
        NotConnected,
        NotLoggedIn,
        Socket,
        IqTimeout,
        IqRefused { code: 500 },
        InvalidRequest,
        Internal,
    ] {
        assert!(
            !refresh_first(failure),
            "{failure:?} must not be tried again inside the attempt (a Socket failure may have left)"
        );
    }
}

/// W5b (brief §2.6; fix F2): an upload that did not finish, a refused authorisation (upstream already asked for a fresh
/// one once), a timeout, a rate limit and a server error are tried again, each AFTER A FLOOR (a retry is a whole new
/// upload under a fresh key, and after a 401 one more `w:m` IQ: the core's own curve starts at 0 ms): 429 five minutes as
/// `RateLimited`, 401/403/408/5xx/`Incomplete` thirty seconds, `NoClient` none (no request left the PC). 413 and every
/// other status are not tried again, with the status in the detail and nothing else. Mutants: 413 retryable; 401 not
/// retryable; one floor dropped (5xx back to none).
#[test]
fn an_upload_is_tried_again_after_its_floor_unless_the_media_server_refused_the_file() {
    let thirty = Some(Duration::from_secs(30));
    for failure in [
        UploadFailure::Incomplete,
        UploadFailure::Status(401),
        UploadFailure::Status(403),
        UploadFailure::Status(408),
        UploadFailure::Status(500),
        UploadFailure::Status(503),
        UploadFailure::Status(599),
    ] {
        let error = upload_error(failure);
        assert_eq!(
            error.kind,
            ErrorKind::Transient {
                retry_after: thirty
            },
            "{failure:?}"
        );
        assert!(error.is_retryable(), "{failure:?}");
    }
    let limited = upload_error(UploadFailure::Status(429));
    assert_eq!(
        limited.kind,
        ErrorKind::RateLimited {
            retry_after: Some(Duration::from_secs(300))
        },
        "the CDN's 429 waits as the chat server's does"
    );
    assert!(limited.is_retryable());
    assert_eq!(
        upload_error(UploadFailure::NoClient).kind,
        ErrorKind::Transient { retry_after: None },
        "nothing left the PC: no floor"
    );
    for code in [400, 404, 410, 413, 415, 600] {
        let error = upload_error(UploadFailure::Status(code));
        assert_eq!(error.kind, ErrorKind::Unsupported, "code {code}");
        assert!(!error.is_retryable(), "code {code}");
        assert!(error.detail.ends_with(&format!("(code {code})")));
    }
    assert_eq!(
        upload_error(UploadFailure::Status(401)).detail,
        "the media server refused the upload's authorisation (code 401)"
    );
}
