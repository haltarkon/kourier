//! T-1: one case per event kind of the W2 table and per shape of a logout. The events are built
//! with upstream's own builders, as the client dispatches them.

use std::time::Duration;

use kourier_model::{ConvKey, UserId};
use wacore::chrono::Duration as ChronoDuration;
use wacore::net::DisconnectReason;
use wacore::pair_code::PairCodeRejection;
use wacore::types::events::{
    ClientOutdated, ConnectFailure, ConnectFailureReason, Connected, Disconnected, Event,
    LoggedOut, LogoutMessage, OfflineSyncCompleted, OfflineSyncInterrupted, OfflineSyncPreview,
    PairError, PairPasskeyConfirmation, PairPasskeyError, PairPasskeyRequest, PairSuccess,
    PairingCode, PairingCodeError, PairingCodeRefresh, PairingQrCode, PairingQrCodesExhausted,
    QrScannedWithoutMultidevice, ReachoutTimelock, ReachoutTimelockUpdate, StreamError,
    StreamReplaced, TempBanReason, TemporaryBan,
};
use wacore_binary::builder::NodeBuilder;
use wacore_binary::{Jid, Node};

use super::*;

const NOW: u64 = 1_700_000_000_000;

/// `durable` with the test clock and an account whose ids are not known.
fn durable(event: &Event) -> Option<Vec<BackendEvent>> {
    super::durable(event, NOW, &OwnIds::default())
}

fn t(event: Event) -> Option<WaSignal> {
    translate(&event, NOW)
}

fn failure(reason: i32) -> Node {
    NodeBuilder::new("failure")
        .attr("reason", reason.to_string())
        .build()
}

fn logged_out(reason: ConnectFailureReason, raw: Option<Node>) -> Event {
    let builder = LoggedOut::builder()
        .on_connect(raw.as_ref().is_some_and(|n| n.tag == "failure"))
        .reason(reason);
    Event::LoggedOut(Box::new(match raw {
        Some(raw) => builder.raw(raw).build(),
        None => builder.build(),
    }))
}

fn stream_error(code: &'static str) -> Node {
    NodeBuilder::new("stream:error").attr("code", code).build()
}

fn shape_of(signal: Option<WaSignal>) -> LogoutShape {
    match signal {
        Some(WaSignal::LoggedOut(l)) => l.shape,
        other => panic!("not a server logout: {other:?}"),
    }
}

// ----- the connection -----

#[test]
fn connected_and_disconnected() {
    assert_eq!(
        t(Event::Connected(Connected::builder().build())),
        Some(WaSignal::Connected)
    );
    for (reason, clean) in [
        (DisconnectReason::StreamEnded, true),
        (DisconnectReason::ReadError("x".into()), false),
    ] {
        assert_eq!(
            t(Event::Disconnected(
                Disconnected::builder().reason(reason).build()
            )),
            Some(WaSignal::Disconnected { clean })
        );
    }
}

#[test]
fn a_connect_failure_upstream_retries_is_a_retry_and_the_others_are_refusals() {
    let cf = |reason| Event::ConnectFailure(ConnectFailure::builder().reason(reason).build());
    assert_eq!(
        t(cf(ConnectFailureReason::InternalServerError)),
        Some(WaSignal::WillRetry(RetryWhy::ServerError))
    );
    assert_eq!(
        t(cf(ConnectFailureReason::ServiceUnavailable)),
        Some(WaSignal::WillRetry(RetryWhy::Unavailable))
    );
    // Terminal for the client: never reported as something it will retry.
    assert_eq!(
        t(cf(ConnectFailureReason::Generic)),
        Some(WaSignal::Refused { code: 400 })
    );
    assert_eq!(
        t(cf(ConnectFailureReason::BadUserAgent)),
        Some(WaSignal::Refused { code: 409 })
    );
    // A 402 whose code or time upstream could not read comes as a plain failure.
    assert_eq!(
        t(cf(ConnectFailureReason::TempBanned)),
        Some(WaSignal::Refused { code: 402 })
    );
    assert_eq!(
        t(cf(ConnectFailureReason::Unknown(77))),
        Some(WaSignal::Refused { code: 77 })
    );
}

#[test]
fn outdated_replaced_and_the_rate_limit() {
    assert_eq!(
        t(Event::ClientOutdated(ClientOutdated::builder().build())),
        Some(WaSignal::Outdated)
    );
    assert_eq!(
        t(Event::ConnectFailure(
            ConnectFailure::builder()
                .reason(ConnectFailureReason::ClientOutdated)
                .build()
        )),
        Some(WaSignal::Outdated)
    );
    assert_eq!(
        t(Event::StreamReplaced(StreamReplaced::builder().build())),
        Some(WaSignal::Replaced)
    );
    let stream = |code: &str| {
        t(Event::StreamError(
            StreamError::builder().code(code.to_owned()).build(),
        ))
    };
    assert_eq!(
        stream("429"),
        Some(WaSignal::WillRetry(RetryWhy::RateLimited))
    );
    // A routing failure of one stanza (no code, or any other) is informational.
    assert_eq!(stream(""), None);
    assert_eq!(stream("503"), None);
}

// ----- a temporary ban -----

fn ban(code: i32, secs: i64, message: Option<&str>, url: Option<&str>) -> Event {
    Event::TemporaryBan(Box::new(
        TemporaryBan::builder()
            .code(TempBanReason::from(code))
            .expire(ChronoDuration::try_seconds(secs).expect("seconds"))
            .maybe_message(message.map(str::to_owned))
            .maybe_url(url.map(str::to_owned))
            .build(),
    ))
}

#[test]
fn a_ban_lasts_its_duration_from_now_not_until_it() {
    let Some(WaSignal::Banned(b)) = t(ban(104, 86_400, None, None)) else {
        panic!("not a ban");
    };
    assert_eq!(b.ban, WaBan::SameMessageToMany);
    // The wire's `expire` is a duration: the end is the time it was heard plus a day.
    assert_eq!(b.until_ms, NOW + 86_400_000);
    assert_eq!(b.explanation, None);
    assert_eq!(b.appeal_url, None);
}

#[test]
fn every_ban_code_has_its_name_and_an_unknown_one_keeps_its_number() {
    for (code, ban_name) in [
        (101, WaBan::TooManyStrangers),
        (102, WaBan::ManyBlocked),
        (103, WaBan::TooManyGroups),
        (104, WaBan::SameMessageToMany),
        (106, WaBan::BroadcastList),
        (105, WaBan::Other(105)),
        (999, WaBan::Other(999)),
    ] {
        let Some(WaSignal::Banned(b)) = t(ban(code, 60, None, None)) else {
            panic!("not a ban");
        };
        assert_eq!(b.ban, ban_name, "code {code}");
        assert_eq!(b.ban.code(), code);
    }
}

#[test]
fn a_ban_keeps_whatsapps_sentence_and_only_an_https_page() {
    let Some(WaSignal::Banned(b)) = t(ban(
        101,
        60,
        Some("  You sent\ttoo many\nmessages.  "),
        Some("https://faq.example.invalid/ban"),
    )) else {
        panic!("not a ban");
    };
    assert_eq!(
        b.explanation.as_deref(),
        Some("You sent too many messages.")
    );
    assert_eq!(
        b.appeal_url.as_deref(),
        Some("https://faq.example.invalid/ban")
    );
    for bad in [
        "http://faq.example.invalid/ban",
        "javascript:alert(1)",
        "https://good.example@evil.example/",
    ] {
        let Some(WaSignal::Banned(b)) = t(ban(101, 60, None, Some(bad))) else {
            panic!("not a ban");
        };
        assert_eq!(b.appeal_url, None, "{bad}");
    }
}

// ----- a logout: one case per shape -----

#[test]
fn a_logout_without_a_stanza_is_our_own_and_not_a_server_logout() {
    // `Client::logout` dispatches `LoggedOut` with no stanza before its shutdown (critic H8).
    assert_eq!(
        t(logged_out(ConnectFailureReason::LoggedOut, None)),
        Some(WaSignal::LocalLogout)
    );
}

#[test]
fn the_shapes_of_a_logout_by_the_server() {
    let device_removed = NodeBuilder::new("stream:error")
        .children([NodeBuilder::new("conflict")
            .attr("type", "device_removed")
            .build()])
        .build();
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::LoggedOut,
            Some(device_removed)
        ))),
        LogoutShape::RemovedByPhone
    );
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::LoggedOut,
            Some(stream_error("516"))
        ))),
        LogoutShape::RemovedByPhone
    );
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::LoggedOut,
            Some(stream_error("401"))
        ))),
        LogoutShape::Unauthorized
    );
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::LoggedOut,
            Some(failure(401))
        ))),
        LogoutShape::Unauthorized
    );
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::AccountLocked,
            Some(failure(403))
        ))),
        LogoutShape::Locked
    );
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::UnknownLogout,
            Some(failure(406))
        ))),
        LogoutShape::Refused406
    );
    // A stanza of a kind nobody knows is not guessed at.
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::LoggedOut,
            Some(stream_error("999"))
        ))),
        LogoutShape::Other
    );
    assert_eq!(
        shape_of(t(logged_out(
            ConnectFailureReason::LoggedOut,
            Some(NodeBuilder::new("iq").build())
        ))),
        LogoutShape::Other
    );
}

#[test]
fn a_lock_keeps_its_violation_reason_as_a_token_and_never_its_appeal_token() {
    let raw = NodeBuilder::new("failure")
        .attr("reason", "403")
        .attr("violation_reason", "Other_Harm")
        .attr("vt", "1")
        .attr("appeal_token", "0aFICTITIOUSappealTOKEN00")
        .build();
    let event = Event::LoggedOut(Box::new(
        LoggedOut::builder()
            .on_connect(true)
            .reason(ConnectFailureReason::AccountLocked)
            .logout_message(
                LogoutMessage::builder()
                    .header("Account locked".to_owned())
                    .subtext("Open WhatsApp on\nyour phone.".to_owned())
                    .build(),
            )
            .raw(raw)
            .build(),
    ));
    let signal = t(event).expect("signal");
    let WaSignal::LoggedOut(l) = &signal else {
        panic!("not a logout");
    };
    assert_eq!(l.shape, LogoutShape::Locked);
    assert_eq!(l.violation.as_deref(), Some("other_harm"));
    assert_eq!(
        l.explanation.as_deref(),
        Some("Account locked Open WhatsApp on your phone.")
    );
    // Nothing of the one-time token is kept anywhere in the signal.
    assert!(!format!("{signal:?}").contains("appealTOKEN"));
}

#[test]
fn a_violation_reason_that_is_not_a_token_is_not_kept() {
    let raw = NodeBuilder::new("failure")
        .attr("reason", "403")
        .attr("violation_reason", "call me on +15555550100")
        .build();
    let signal = t(logged_out(ConnectFailureReason::AccountLocked, Some(raw)));
    let Some(WaSignal::LoggedOut(l)) = signal else {
        panic!("not a logout");
    };
    assert_eq!(l.violation, None);
}

// ----- the reach-out lock -----

fn timelock(is_active: Option<bool>, ends: Option<&str>, kind: Option<&str>) -> ReachoutTimelock {
    ReachoutTimelock {
        enforcement_type: kind.map(str::to_owned),
        is_active,
        time_enforcement_ends: ends.map(str::to_owned),
    }
}

fn push(state: ReachoutTimelock) -> Option<WaSignal> {
    t(Event::ReachoutTimelockUpdate(
        ReachoutTimelockUpdate::builder().state(state).build(),
    ))
}

#[test]
fn an_active_lock_ends_at_the_epoch_seconds_the_server_states() {
    assert_eq!(
        push(timelock(Some(true), Some("1704153600"), Some("DEFAULT"))),
        Some(WaSignal::Hold(HoldState::Active {
            until_ms: Some(1_704_153_600_000),
            kind: Some("default".to_owned()),
        }))
    );
}

#[test]
fn an_active_lock_without_a_readable_end_is_a_lock_without_a_time() {
    for ends in [
        None,
        Some("soon"),
        Some(""),
        Some("-5"),
        Some("99999999999999999999"),
    ] {
        assert_eq!(
            push(timelock(Some(true), ends, None)),
            Some(WaSignal::Hold(HoldState::Active {
                until_ms: None,
                kind: None
            })),
            "{ends:?}"
        );
    }
}

#[test]
fn a_lifted_lock_is_lifted_and_an_update_that_asserts_nothing_gives_no_signal() {
    assert_eq!(
        push(timelock(Some(false), Some("1704153600"), None)),
        Some(WaSignal::Hold(HoldState::Lifted))
    );
    // `is_active: None` makes no assertion: it is NOT a lifted lock.
    assert_eq!(push(timelock(None, Some("1704153600"), None)), None);
    assert_eq!(hold_of(&timelock(None, None, None)), HoldState::Unasserted);
    // The pull reads the same rule as the push.
    assert_eq!(
        hold_of(&timelock(Some(true), Some("60"), None)),
        HoldState::Active {
            until_ms: Some(60_000),
            kind: None
        }
    );
}

// ----- linking -----

#[test]
fn the_qr_code_keeps_its_validity_and_hides_itself() {
    let signal = t(Event::PairingQrCode(
        PairingQrCode::builder()
            .code("2@SECRETQRCONTENTS".to_owned())
            .timeout(Duration::from_secs(60))
            .build(),
    ))
    .expect("signal");
    assert_eq!(
        signal,
        WaSignal::QrCode {
            code: Redacted::new("2@SECRETQRCONTENTS".to_owned()),
            valid_for: Duration::from_secs(60),
        }
    );
    assert!(!format!("{signal:?}").contains("SECRETQR"));
    assert_eq!(
        t(Event::PairingQrCodesExhausted(
            PairingQrCodesExhausted::builder()
                .disconnected(true)
                .build()
        )),
        Some(WaSignal::QrExhausted {
            socket_closed: true
        })
    );
    assert_eq!(
        t(Event::QrScannedWithoutMultidevice(
            QrScannedWithoutMultidevice::builder().build()
        )),
        Some(WaSignal::QrScannedWithoutMultidevice)
    );
}

#[test]
fn the_pair_code_hides_itself_and_its_refresh_says_whether_it_is_manual() {
    let signal = t(Event::PairingCode(
        PairingCode::builder()
            .code("ABCD1234".to_owned())
            .timeout(Duration::from_secs(180))
            .build(),
    ))
    .expect("signal");
    assert_eq!(
        signal,
        WaSignal::PairCode {
            code: Redacted::new("ABCD1234".to_owned()),
            valid_for: Duration::from_secs(180),
        }
    );
    assert!(!format!("{signal:?}").contains("ABCD1234"));
    for manual in [true, false] {
        assert_eq!(
            t(Event::PairingCodeRefresh(
                PairingCodeRefresh::builder().force_manual(manual).build()
            )),
            Some(WaSignal::PairCodeRefresh {
                force_manual: manual
            })
        );
    }
}

fn pair_code_error(
    rejection: Option<PairCodeRejection>,
    backoff: Option<Duration>,
) -> Option<WaSignal> {
    t(Event::PairingCodeError(
        PairingCodeError::builder()
            .maybe_rejection(rejection)
            .maybe_backoff(backoff)
            .error("pair code failed".to_owned())
            .build(),
    ))
}

#[test]
fn a_pair_code_refusal_keeps_the_servers_code_and_backoff() {
    for (rejection, refusal, throttle) in [
        (PairCodeRejection::BadRequest, PairRefusal::BadRequest, true),
        (PairCodeRejection::Forbidden, PairRefusal::Forbidden, false),
        (
            PairCodeRejection::RateOverlimit,
            PairRefusal::RateOverlimit,
            true,
        ),
        (
            PairCodeRejection::FeatureNotAvailable,
            PairRefusal::NotAvailable,
            false,
        ),
        (
            PairCodeRejection::InternalServerError,
            PairRefusal::ServerError,
            false,
        ),
        (
            PairCodeRejection::Unknown(418),
            PairRefusal::Other(418),
            false,
        ),
    ] {
        let backoff = Some(Duration::from_secs(30));
        assert_eq!(
            pair_code_error(Some(rejection), backoff),
            Some(WaSignal::PairCodeRefused {
                refusal: Some(refusal),
                backoff
            })
        );
        assert_eq!(refusal.is_throttle(), throttle, "{refusal:?}");
    }
    // No answer from the server (local validation, no connection, a timeout).
    assert_eq!(
        pair_code_error(None, None),
        Some(WaSignal::PairCodeRefused {
            refusal: None,
            backoff: None
        })
    );
}

#[test]
fn a_finished_pairing_names_the_two_ids_and_prints_neither() {
    let mut id = Jid::pn("15555550100");
    id.device = 7;
    let signal = t(Event::PairSuccess(
        PairSuccess::builder()
            .id(id)
            .lid(Jid::lid("100000000000001"))
            .business_name(String::new())
            .platform("android".to_owned())
            .build(),
    ))
    .expect("signal");
    let WaSignal::PairSuccess(p) = &signal else {
        panic!("not a pairing");
    };
    assert_eq!(p.pn.expose(), "15555550100");
    assert_eq!(p.lid.expose(), "100000000000001");
    assert_eq!(p.device, 7);
    let printed = format!("{signal:?}");
    assert!(!printed.contains("15555550100") && !printed.contains("100000000000001"));
}

#[test]
fn a_failed_pairing_and_the_passkey_events() {
    assert_eq!(
        t(Event::PairError(
            PairError::builder()
                .id(Jid::pn("15555550100"))
                .lid(Jid::lid("100000000000001"))
                .business_name(String::new())
                .platform(String::new())
                .error("bad signature".to_owned())
                .build()
        )),
        Some(WaSignal::PairFailed)
    );
    assert_eq!(
        t(Event::PairPasskeyRequest(
            PairPasskeyRequest::builder()
                .request_options_json("{\"challenge\":\"SECRETCHALLENGE\"}".to_owned())
                .build()
        )),
        Some(WaSignal::PasskeyRequested)
    );
    let confirmation = t(Event::PairPasskeyConfirmation(
        PairPasskeyConfirmation::builder()
            .code("ABCD-1234".to_owned())
            .skip_handoff_ux(false)
            .build(),
    ))
    .expect("signal");
    assert!(!format!("{confirmation:?}").contains("ABCD"));
    assert_eq!(
        t(Event::PairPasskeyError(
            PairPasskeyError::builder()
                .error("x".to_owned())
                .continuation(true)
                .build()
        )),
        Some(WaSignal::PasskeyFailed { continuation: true })
    );
}

#[test]
fn an_event_w2_does_not_act_on_gives_no_signal() {
    // A stream error that is no rate limit is a routing failure of one stanza.
    assert_eq!(
        t(Event::StreamError(
            StreamError::builder().code("503".to_owned()).build()
        )),
        None
    );
}

/// The offline drain's three events (W3a, chunk C7): the preview's `total` and the completion's
/// `count` are kept as they are (fix F7: the catch-up machine compares them to tell a drain upstream
/// gave up waiting for); `total` counts stanzas of every kind, so it is no progress fraction.
#[test]
fn the_offline_drain_has_three_signals_and_two_of_them_carry_a_number() {
    assert_eq!(
        t(Event::OfflineSyncPreview(
            OfflineSyncPreview::builder()
                .total(12)
                .app_data_changes(1)
                .messages(7)
                .notifications(1)
                .receipts(2)
                .calls(0)
                .statuses(1)
                .build()
        )),
        Some(WaSignal::OfflinePreview { total: 12 })
    );
    assert_eq!(
        t(Event::OfflineSyncCompleted(
            OfflineSyncCompleted::builder().count(3).build()
        )),
        Some(WaSignal::OfflineDone { count: 3 })
    );
    assert_eq!(
        t(Event::OfflineSyncInterrupted(
            OfflineSyncInterrupted::builder()
                .total(12)
                .delivered(5)
                .build()
        )),
        Some(WaSignal::OfflineInterrupted)
    );
    // None of them is a connection state by itself: the catch-up machine decides.
    for signal in [
        WaSignal::OfflinePreview { total: 1 },
        WaSignal::OfflineDone { count: 1 },
        WaSignal::OfflineInterrupted,
    ] {
        assert_eq!(crate::states::state_of(&signal), None, "{signal:?}");
    }
}

// ----- the helpers -----

#[test]
fn an_explanation_is_one_plain_bounded_line() {
    assert_eq!(explanation(""), None);
    assert_eq!(explanation(" \n\t "), None);
    assert_eq!(
        explanation("a\u{0}b \u{202e}c\n\n d").as_deref(),
        Some("a b c d")
    );
    let long = "x".repeat(1000);
    assert_eq!(
        explanation(&long).map(|s| s.chars().count()),
        Some(EXPLANATION_MAX)
    );
    // A cut never leaves a trailing space.
    let spaced = format!("{} y", "x".repeat(EXPLANATION_MAX - 1));
    let cut = explanation(&spaced).expect("text");
    assert_eq!(cut, "x".repeat(EXPLANATION_MAX - 1));
}

#[test]
fn only_a_bounded_https_link_is_kept() {
    assert!(https_url("https://example.invalid/a?b=c#d").is_some());
    assert!(https_url("https://example.invalid").is_some());
    for bad in [
        "",
        "http://example.invalid/",
        "HTTPS://example.invalid/",
        "https://",
        "https:///path",
        "https://example.invalid/a b",
        "https://example.invalid/\u{202e}",
        "https://user@example.invalid/",
        "https://example.invalid\\@evil.invalid/",
        "ftp://example.invalid/",
    ] {
        assert_eq!(https_url(bad), None, "{bad:?}");
    }
    assert_eq!(
        https_url(&format!("https://example.invalid/{}", "a".repeat(URL_MAX))),
        None
    );
}

#[test]
fn a_token_is_safe_to_log_or_it_is_not_kept() {
    assert_eq!(safe_token("other_harm").as_deref(), Some("other_harm"));
    assert_eq!(safe_token("Mixed-Case.1").as_deref(), Some("mixed-case.1"));
    for bad in [
        "",
        "two words",
        "+15555550100",
        "id1234567",
        "ünï",
        &"x".repeat(49),
    ] {
        assert_eq!(safe_token(bad), None, "{bad:?}");
    }
    assert!(
        safe_token("v12345").is_some(),
        "five digits are not a number"
    );
}

// ----- the inbox row of a message (W3a, chunk C2) -----

fn inbound(offline: bool, lid: bool, timer: Option<u32>) -> InboundMessage {
    use wacore::types::message::{MessageInfo, MessageSource};
    let mut message = waproto::whatsapp::Message::default();
    message.conversation = Some("synthetic text".to_owned());
    let mut item = InboundMessage::builder()
        .message(std::sync::Arc::new(message))
        .info(std::sync::Arc::new(MessageInfo {
            id: "SYNTH-ID-1".into(),
            source: MessageSource {
                chat: "100@g.us".parse().expect("jid"),
                sender: "200@lid".parse().expect("jid"),
                sender_alt: Some("31000000000@s.whatsapp.net".parse().expect("jid")),
                recipient_alt: Some("31000000001@s.whatsapp.net".parse().expect("jid")),
                is_group: true,
                is_from_me: false,
                addressing_mode: lid.then_some(AddressingMode::Lid),
                ..MessageSource::default()
            },
            push_name: "Synthetic Name".into(),
            is_offline: offline,
            ..MessageInfo::default()
        }))
        .build();
    item.ephemeral_expiration = timer;
    item
}

#[test]
fn a_message_becomes_the_inbox_row_stage_two_needs() {
    let row = arrival(&inbound(true, true, Some(86_400)), Origin::E2e);
    assert_eq!(row.chat, "100@g.us");
    assert_eq!(row.sender, "200@lid");
    assert_eq!(row.id, "SYNTH-ID-1");
    assert_eq!(row.origin, Origin::E2e);
    assert_eq!(row.meta.v, InboxMeta::VERSION);
    assert!(row.meta.is_offline && row.meta.is_group && row.meta.lid_addressing);
    assert!(!row.meta.from_me);
    assert_eq!(row.meta.ephemeral_expiration, Some(86_400));
    assert_eq!(row.meta.push_name, "Synthetic Name");
    assert_eq!(
        row.meta.sender_alt.as_deref(),
        Some("31000000000@s.whatsapp.net")
    );
    assert_eq!(
        row.meta.recipient_alt.as_deref(),
        Some("31000000001@s.whatsapp.net"),
        "the recipient's other id rides in the row (W3b: only a message of ours vouches a pair with it)"
    );
    let back = waproto::codec::message_decode(&row.message).expect("the message round-trips");
    assert_eq!(back.conversation.as_deref(), Some("synthetic text"));
}

#[test]
fn the_defaults_of_a_live_phone_number_addressed_message() {
    let row = arrival(&inbound(false, false, None), Origin::Pdo);
    assert_eq!(row.origin, Origin::Pdo);
    assert!(!row.meta.is_offline && !row.meta.lid_addressing);
    assert_eq!(row.meta.ephemeral_expiration, None);
}

#[test]
fn the_meta_survives_its_own_format_and_a_later_field_has_a_default() {
    let row = arrival(&inbound(true, true, Some(60)), Origin::E2e);
    let bytes = serde_json::to_vec(&row.meta).expect("encode");
    let back: InboxMeta = serde_json::from_slice(&bytes).expect("decode");
    assert_eq!(back, row.meta);
    // A row written by an older layout (fewer fields) still reads.
    let old: InboxMeta = serde_json::from_slice(br#"{"v":1,"timestamp":5}"#).expect("old layout");
    assert_eq!(
        (old.timestamp, old.from_me, old.is_offline),
        (5, false, false)
    );
    assert_eq!(
        old.recipient_alt, None,
        "a row committed before W3b vouches no pair"
    );
}

// ----- the durable kinds and the items the hook did not commit (W3a, chunk C6) -----

fn receipt(chat: &str, sender: &str, kind: ReceiptType, ids: &[&str]) -> Event {
    use wacore::types::message::MessageSource;
    Event::Receipt(
        Receipt::builder()
            .source(MessageSource {
                chat: chat.parse().expect("jid"),
                sender: sender.parse().expect("jid"),
                ..MessageSource::default()
            })
            .message_ids(ids.iter().map(|id| (*id).into()).collect())
            .timestamp(wacore::chrono::DateTime::from_timestamp(1_700_000_005, 0).expect("time"))
            .r#type(kind)
            .offline(false)
            .build(),
    )
}

#[test]
fn a_delivery_read_or_played_receipt_is_a_receipts_event_for_our_own_messages_of_that_chat() {
    for (kind, want) in [
        (ReceiptType::Delivered, ReceiptKind::Delivered),
        (ReceiptType::Read, ReceiptKind::Read),
        (ReceiptType::Played, ReceiptKind::Viewed),
    ] {
        let event = receipt(
            "31000000001@s.whatsapp.net",
            "31000000001@s.whatsapp.net",
            kind,
            &["3EB0AA", "3EB0BB"],
        );
        let durable = durable(&event).expect("a durable kind");
        let [
            BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            },
        ] = durable.as_slice()
        else {
            panic!("not one Receipts event: {durable:?}")
        };
        assert_eq!(*kind, want);
        assert_eq!(*by, UserId::from_parts("pn", "31000000001").unwrap());
        assert_eq!(*at, 1_700_000_005_000);
        assert_eq!(targets.len(), 2);
        for (target, id) in targets.iter().zip(["3EB0AA", "3EB0BB"]) {
            assert_eq!(&*target.local, id);
            assert_eq!(target.author, TargetAuthor::Own);
            assert_eq!(
                target.conv,
                Some(ConvKey::Direct(
                    UserId::from_parts("pn", "31000000001").unwrap()
                ))
            );
        }
    }
}

#[test]
fn a_group_receipt_names_the_group_and_its_participant() {
    let event = receipt(
        "120363000000000001@g.us",
        "99000000002@lid",
        ReceiptType::Read,
        &["3EB0CC"],
    );
    let durable = durable(&event).expect("durable");
    let [BackendEvent::Receipts { targets, by, .. }] = durable.as_slice() else {
        panic!("{durable:?}")
    };
    assert_eq!(*by, UserId::from_parts("lid", "99000000002").unwrap());
    assert_eq!(
        targets[0].conv,
        Some(ConvKey::Group("wag:120363000000000001".into()))
    );
}

/// Fix F8 (protocol review P4): a receipt's actor with the other id the stanza vouches for
/// (`participant_pn`/`participant_lid`, upstream's `source.sender_alt`) is stated as an `IdentityMerged`
/// FIRST, by the rules of every other pair: one number and one LID, never one of our own ids. Without it
/// a member's receipt after the LID migration waits two days for a merge nobody states.
#[test]
fn a_receipts_vouched_pair_is_merged_first_and_never_for_our_own_ids() {
    let with_alt = |sender: &str, alt: Option<&str>| {
        let Event::Receipt(mut receipt) = receipt(
            "120363000000000001@g.us",
            sender,
            ReceiptType::Read,
            &["3EB0AA"],
        ) else {
            unreachable!()
        };
        receipt.source.sender_alt = alt.map(|alt| alt.parse().expect("jid"));
        Event::Receipt(receipt)
    };
    let ours = OwnIds {
        lid: Some(UserId::from_parts("lid", "99900100123").unwrap()),
        pn: Some(UserId::from_parts("pn", "15550100123").unwrap()),
    };
    let pn = UserId::from_parts("pn", "15550100101").unwrap();
    let lid = UserId::from_parts("lid", "99900100101").unwrap();
    // The actor is the LID, the stanza vouches his number: merged first, then the receipt by the LID.
    let events = super::durable(
        &with_alt("99900100101@lid", Some("15550100101@s.whatsapp.net")),
        NOW,
        &ours,
    )
    .expect("durable");
    let [
        BackendEvent::IdentityMerged { from, into },
        BackendEvent::Receipts { by, .. },
    ] = events.as_slice()
    else {
        panic!("a merge, then the receipt: {events:?}")
    };
    assert_eq!((from, into, by), (&pn, &lid, &lid));
    // The other way round (the actor is the number) states the same pair, the hosted namespaces included.
    let events = super::durable(
        &with_alt("15550100101@hosted", Some("99900100101@hosted.lid")),
        NOW,
        &ours,
    )
    .expect("durable");
    assert!(
        matches!(events.as_slice(), [
            BackendEvent::IdentityMerged { from, into },
            BackendEvent::Receipts { by, .. },
        ] if (from, into, by) == (&pn, &lid, &pn)),
        "{events:?}"
    );
    // No alternate, two ids of one server, a stranger's non-person: only the receipt.
    for alt in [
        None,
        Some("15550100102@s.whatsapp.net"),
        Some("120363000000000009@g.us"),
    ] {
        let events = super::durable(&with_alt("15550100101@s.whatsapp.net", alt), NOW, &ours)
            .expect("durable");
        assert!(
            matches!(events.as_slice(), [BackendEvent::Receipts { .. }]),
            "{alt:?}: {events:?}"
        );
    }
    // Never for our own ids: our LID with our number, or either of them beside another person's id.
    for (sender, alt) in [
        ("99900100123@lid", "15550100123@s.whatsapp.net"),
        ("99900100101@lid", "15550100123@s.whatsapp.net"),
        ("99900100123@lid", "15550100101@s.whatsapp.net"),
    ] {
        let events = super::durable(&with_alt(sender, Some(alt)), NOW, &ours).expect("durable");
        assert!(
            matches!(events.as_slice(), [BackendEvent::Receipts { .. }]),
            "{sender} / {alt}: {events:?}"
        );
    }
}

/// Fix F5: a receipt of a hosted device reads like one of the ordinary namespace, a hosted number as a
/// number and a hosted LID as a LID (the lead's follow-up: a table, so a hosted LID that stopped being a
/// LID fails here and not only in the map's test).
#[test]
fn a_receipt_of_a_hosted_device_is_one_of_the_ordinary_namespace() {
    for (chat, sender, scheme, user) in [
        (
            "15550100101@hosted",
            "15550100101:3@hosted",
            "pn",
            "15550100101",
        ),
        (
            "99900100101@hosted.lid",
            "99900100101:3@hosted.lid",
            "lid",
            "99900100101",
        ),
    ] {
        let events = durable(&receipt(chat, sender, ReceiptType::Delivered, &["3EB0AA"]))
            .unwrap_or_else(|| panic!("{chat}: not durable"));
        let [BackendEvent::Receipts { targets, by, .. }] = events.as_slice() else {
            panic!("{chat}: {events:?}")
        };
        let person = UserId::from_parts(scheme, user).unwrap();
        assert_eq!(*by, person, "{chat}");
        assert_eq!(targets[0].conv, Some(ConvKey::Direct(person)), "{chat}");
    }
}

#[test]
fn receipts_that_say_nothing_about_a_message_are_not_durable() {
    let pn = "31000000001@s.whatsapp.net";
    for kind in [
        ReceiptType::Sent,
        ReceiptType::Sender,
        ReceiptType::Retry,
        // A self type from an actor that is not us says nothing about us (our own read is the
        // `is_from_me` one, tested below).
        ReceiptType::ReadSelf,
        ReceiptType::PlayedSelf,
        ReceiptType::ServerError,
        ReceiptType::Inactive,
        ReceiptType::PeerMsg,
        ReceiptType::HistorySync,
    ] {
        assert!(durable(&receipt(pn, pn, kind, &["3EB0AA"])).is_none());
    }
    // No ids, a newsletter, and a plain type from our own account (upstream folds ours into the
    // self types, so it would be a malformed one).
    assert!(durable(&receipt(pn, pn, ReceiptType::Read, &[])).is_none());
    assert!(
        durable(&receipt(
            "120363000000000002@newsletter",
            pn,
            ReceiptType::Read,
            &["3EB0AA"]
        ))
        .is_none()
    );
    let Event::Receipt(mut own) = receipt(pn, pn, ReceiptType::Read, &["3EB0AA"]) else {
        unreachable!()
    };
    own.source.is_from_me = true;
    assert!(durable(&Event::Receipt(own)).is_none());
    // And the connection's events are not durable.
    assert!(durable(&Event::Connected(Connected::builder().build())).is_none());
}

/// An own-account receipt as upstream addresses it (`address_receipt`): `is_from_me`, the chat is the
/// peer's thread (a person) or the group, the sender is our own account.
fn own_receipt(chat: &str, sender: &str, kind: ReceiptType, ids: &[&str]) -> Event {
    let Event::Receipt(mut receipt) = receipt(chat, sender, kind, ids) else {
        unreachable!()
    };
    receipt.source.is_from_me = true;
    Event::Receipt(receipt)
}

#[test]
fn our_own_read_on_the_phone_is_a_receipt_by_our_id() {
    let our_lid = UserId::from_parts("lid", "99900100123").unwrap();
    let peer = ConvKey::Direct(UserId::from_parts("pn", "31000000001").unwrap());
    for (kind, want) in [
        (ReceiptType::ReadSelf, ReceiptKind::Read),
        (ReceiptType::PlayedSelf, ReceiptKind::Viewed),
    ] {
        let event = own_receipt(
            "31000000001@s.whatsapp.net",
            "99900100123@lid",
            kind,
            &["3EB0AA", "3EB0BB"],
        );
        let durable = durable(&event).expect("a durable kind");
        let [
            BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            },
        ] = durable.as_slice()
        else {
            panic!("not one Receipts event: {durable:?}")
        };
        assert_eq!(*kind, want);
        assert_eq!(*by, our_lid, "the actor is one of OUR ids");
        assert_eq!(*at, 1_700_000_005_000);
        assert_eq!(targets.len(), 2);
        for (target, id) in targets.iter().zip(["3EB0AA", "3EB0BB"]) {
            assert_eq!(&*target.local, id);
            // The messages are somebody else's: the receipt does not say whose.
            assert_eq!(target.author, TargetAuthor::Unknown);
            assert_eq!(target.conv.as_ref(), Some(&peer), "the peer's thread");
        }
    }
    // A group: the conversation is the group, and our own number is as good as our LID.
    let durable = durable(&own_receipt(
        "120363000000000001@g.us",
        "15550100123@s.whatsapp.net",
        ReceiptType::ReadSelf,
        &["3EB0CC"],
    ))
    .expect("durable");
    let [BackendEvent::Receipts { targets, by, .. }] = durable.as_slice() else {
        panic!("{durable:?}")
    };
    assert_eq!(*by, UserId::from_parts("pn", "15550100123").unwrap());
    assert_eq!(
        targets[0].conv,
        Some(ConvKey::Group("wag:120363000000000001".into()))
    );
}

#[test]
fn a_status_receipt_gives_nothing() {
    let pn = "31000000001@s.whatsapp.net";
    for own in [false, true] {
        for kind in [
            ReceiptType::Read,
            ReceiptType::ReadSelf,
            ReceiptType::Played,
            ReceiptType::PlayedSelf,
            ReceiptType::Delivered,
        ] {
            let Event::Receipt(mut status) = receipt("status@broadcast", pn, kind, &["3EB0AA"])
            else {
                unreachable!()
            };
            status.source.is_from_me = own;
            assert!(
                durable(&Event::Receipt(status)).is_none(),
                "the status feed has no conversation"
            );
        }
    }
}

// ----- typing (W3b, chunk C6a) -----

fn presence(chat: &str, sender: &str, group: bool, state: ChatActivity) -> Event {
    use wacore::types::message::MessageSource;
    Event::ChatPresence(
        ChatPresenceUpdate::builder()
            .source(MessageSource {
                chat: chat.parse().expect("jid"),
                sender: sender.parse().expect("jid"),
                is_group: group,
                ..MessageSource::default()
            })
            .state(state)
            .build(),
    )
}

fn typing(signal: Option<WaSignal>) -> Typing {
    match signal {
        Some(WaSignal::Typing(typing)) => typing,
        other => panic!("not a typing signal: {other:?}"),
    }
}

#[test]
fn typing_and_recording_are_on_and_idle_is_off() {
    let pn = "31000000001@s.whatsapp.net";
    let peer = UserId::from_parts("pn", "31000000001").unwrap();
    for (state, on) in [
        (ChatActivity::Typing, true),
        // Decision D3: a voice message being recorded reads as typing until the contract can say so.
        (ChatActivity::RecordingAudio, true),
        (ChatActivity::Idle, false),
    ] {
        let got = typing(t(presence(pn, pn, false, state)));
        assert_eq!(got.on, on, "{state:?}");
        assert_eq!(got.conv, ConvKey::Direct(peer.clone()));
        assert_eq!(got.by, peer);
        assert_eq!(got.device, None);
    }
}

#[test]
fn a_group_member_types_in_the_group_and_his_device_is_kept() {
    let got = typing(t(presence(
        "120363000000000001@g.us",
        "99000000002:7@lid",
        true,
        ChatActivity::Typing,
    )));
    assert_eq!(got.conv, ConvKey::Group("wag:120363000000000001".into()));
    assert_eq!(got.by, UserId::from_parts("lid", "99000000002").unwrap());
    assert_eq!(got.device, Some(7));
}

#[test]
fn a_chat_state_of_no_conversation_gives_no_signal_and_is_never_durable() {
    let pn = "31000000001@s.whatsapp.net";
    for (chat, sender) in [
        ("status@broadcast", pn),
        ("120363000000000002@newsletter", pn),
        // A group's participant that is no person.
        ("120363000000000001@g.us", "120363000000000001@g.us"),
    ] {
        assert_eq!(t(presence(chat, sender, true, ChatActivity::Typing)), None);
    }
    // Typing is ephemeral: the rule-(a) list never holds it.
    assert!(durable(&presence(pn, pn, false, ChatActivity::Typing)).is_none());
}

#[test]
fn a_typing_signal_prints_no_id() {
    let pn = "31000000001@s.whatsapp.net";
    let shown = format!("{:?}", t(presence(pn, pn, false, ChatActivity::Typing)));
    assert!(!shown.contains("31000000001"), "{shown}");
    assert!(shown.contains("on: true"), "{shown}");
}

// ----- the handler's messages (W3b, chunks C6b and C6c) -----

fn group_update(actor: &str, action: wacore::stanza::groups::GroupNotificationAction) -> Event {
    let mut update = wacore::types::events::GroupUpdate::builder()
        .group_jid("120363000000000001@g.us".parse().expect("jid"))
        .notification_id("3178000001".to_owned())
        .action_index(0)
        .timestamp(wacore::chrono::DateTime::from_timestamp(1_700_000_100, 0).expect("time"))
        .is_lid_addressing_mode(true)
        .action(Box::new(action))
        .build();
    update.participant = Some(actor.parse().expect("jid"));
    Event::GroupUpdate(update)
}

#[test]
fn a_group_notification_is_noticed_a_lost_message_is_durable_and_the_handler_names_our_own_acts() {
    use wacore::stanza::groups::GroupNotificationAction as Action;
    let subject = || Action::Subject {
        subject: "A synthetic name".to_owned(),
        subject_owner: None,
        subject_owner_pn: None,
        subject_owner_username: None,
        subject_time: None,
    };
    let ours = OwnIds {
        lid: Some(UserId::from_parts("lid", "99900100123").unwrap()),
        pn: Some(UserId::from_parts("pn", "15550100123").unwrap()),
    };
    // A notice of somebody else's: by him; the rename's title is an act the group's row decides (fix F11, M5).
    let noticed = super::noticed(&group_update("99000000002@lid", subject()), NOW, &ours)
        .expect("a notice is written");
    let [BackendEvent::Message(notice)] = noticed.events.as_slice() else {
        panic!("{noticed:?}")
    };
    assert!(matches!(
        notice.body,
        kourier_model::Body::System(kourier_model::SystemEvent::TitleChanged { by: Some(_), .. })
    ));
    assert_eq!(noticed.acts.len(), 1);
    // The same notification caused by one of OUR ids is our own act: "You".
    let noticed = super::noticed(&group_update("99900100123@lid", subject()), NOW, &ours)
        .expect("a notice is written");
    let [BackendEvent::Message(notice)] = noticed.events.as_slice() else {
        panic!("{noticed:?}")
    };
    assert!(matches!(
        notice.body,
        kourier_model::Body::System(kourier_model::SystemEvent::TitleChanged { by: None, .. })
    ));
    // Without ids the handler cannot tell: it names the actor, never loses the notice.
    assert!(
        super::noticed(
            &group_update("99900100123@lid", subject()),
            NOW,
            &OwnIds::default()
        )
        .is_some()
    );
    // A group's notification is no `durable` kind of its own any more: the writer takes it as a notice, with its acts.
    assert!(durable(&group_update("99000000002@lid", subject())).is_none());
    // A rule has no line but an act; an action nobody maps gives nothing.
    let rule = super::noticed(
        &group_update("99000000002@lid", Action::Announce),
        NOW,
        &ours,
    )
    .expect("an act");
    assert!(rule.events.is_empty() && rule.acts.len() == 1);
    assert!(
        super::noticed(
            &group_update(
                "99000000002@lid",
                Action::Invite {
                    code: "x".to_owned()
                }
            ),
            NOW,
            &ours
        )
        .is_none()
    );
    // Nothing else is a notice.
    assert!(super::noticed(&Event::Connected(Connected::builder().build()), NOW, &ours).is_none());
    // A message that cannot be read.
    let lost = Event::UndecryptableMessage(
        wacore::types::events::UndecryptableMessage::builder()
            .info(std::sync::Arc::new(wacore::types::message::MessageInfo {
                id: "3EB0DEAD".into(),
                source: wacore::types::message::MessageSource {
                    chat: "31000000002@s.whatsapp.net".parse().expect("jid"),
                    sender: "31000000002@s.whatsapp.net".parse().expect("jid"),
                    ..Default::default()
                },
                ..Default::default()
            }))
            .is_unavailable(false)
            .unavailable_type(wacore::types::events::UnavailableType::Unknown)
            .decrypt_fail_mode(wacore::types::events::DecryptFailMode::Show)
            .build(),
    );
    let events = durable(&lost).expect("a placeholder is durable");
    assert!(matches!(
        events.as_slice(),
        [BackendEvent::Message(m)] if &*m.local == "3EB0DEAD"
    ));
}

fn batch(hook_committed: bool, chat: &str) -> Event {
    use wacore::types::events::{BatchOrigin, MessageBatch};
    let mut item = inbound(false, false, None);
    let mut info = (*item.info).clone();
    info.source.chat = chat.parse().expect("jid");
    item.info = std::sync::Arc::new(info);
    Event::Messages(
        MessageBatch::builder()
            .messages(std::sync::Arc::from(vec![item]))
            .origin(BatchOrigin::Live)
            .hook_committed(hook_committed)
            .build(),
    )
}

#[test]
fn only_the_items_the_hook_did_not_commit_become_inbox_rows_of_the_handler() {
    // What the hook committed is in the inbox already.
    assert!(unhooked(&batch(true, "100@g.us")).is_empty());
    assert!(unhooked(&batch(true, "120363000000000002@newsletter")).is_empty());
    // A newsletter's items and a recovered message are the handler's, at most once.
    let news = unhooked(&batch(false, "120363000000000002@newsletter"));
    assert_eq!(news.len(), 1);
    assert_eq!(news[0].origin, Origin::Newsletter);
    assert_eq!(news[0].id, "SYNTH-ID-1");
    let recovered = unhooked(&batch(false, "31000000001@s.whatsapp.net"));
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].origin, Origin::Pdo);
    // Other events carry no messages.
    assert!(unhooked(&Event::Connected(Connected::builder().build())).is_empty());
}

#[test]
fn the_dirty_rows_names_become_collections_and_a_name_upstream_does_not_know_is_left_out() {
    use wacore::appstate::patch_decode::WAPatchName;
    let names = [
        "regular",
        "critical_block",
        "unknown",
        "a_future_collection",
    ]
    .map(String::from);
    assert_eq!(
        super::collections_of(&names),
        [WAPatchName::Regular, WAPatchName::CriticalBlock]
    );
}

#[test]
fn a_report_names_the_collections_it_synced_and_the_ones_the_server_refused_by_their_stored_names()
{
    use wacore::appstate::patch_decode::WAPatchName;
    let mut report = whatsapp_rust::AppStateResyncReport::default();
    report.synced.push(WAPatchName::RegularLow);
    report.retryable.push(WAPatchName::Regular);
    report.fatal.push(WAPatchName::CriticalBlock);
    report.skipped.push(WAPatchName::RegularHigh);
    let told = super::report_of(&report);
    assert_eq!(told.synced, ["regular_low"]);
    assert_eq!(told.fatal, ["critical_block"], "a refusal is a fatal one");
}

/// INTEROP-9 (F9): a resync call that WENT OUT and failed (`Internal`: it timed out, or a response could not be read) is
/// an unsynced collection, so the start-up run goes on to the next one; only when NOTHING was asked (`NotConnected`, a
/// request upstream refused to build, a task that died) does the call fail and the run end. A report is told as it is.
/// Kills "every error is a failed call" and "an `Internal` error ends the run".
#[test]
fn a_resync_that_went_out_and_failed_is_unsynced_and_one_never_asked_is_a_failed_call() {
    use super::resync_answer;
    use wacore::appstate::patch_decode::WAPatchName;
    use whatsapp_rust::AppStateError;

    let unsynced = resync_answer(Some(Err(AppStateError::Internal(anyhow::anyhow!(
        "a fixture's failure"
    )))))
    .expect("the call went out");
    assert!(
        unsynced.synced.is_empty() && unsynced.fatal.is_empty(),
        "unsynced: neither synced nor refused"
    );

    for never_asked in [
        resync_answer(Some(Err(AppStateError::NotConnected))),
        resync_answer(Some(Err(AppStateError::InvalidRequest("a fixture".into())))),
        resync_answer(None),
    ] {
        assert!(never_asked.is_err(), "nothing was asked");
    }

    let mut report = whatsapp_rust::AppStateResyncReport::default();
    report.synced.push(WAPatchName::RegularLow);
    let told = resync_answer(Some(Ok(report))).expect("a report");
    assert_eq!(told.synced, ["regular_low"]);
}

/// W4 fix F7: only OUR OWN read or view on another device names a chat for the history's hint; a receipt of somebody
/// else's read, a plain type, one without ids and every other event do not.
/// Mutant for the re-check: `read_elsewhere` answering `None`, or ignoring `is_from_me`.
#[test]
fn only_our_own_read_on_the_phone_names_a_chat_read_elsewhere() {
    use super::read_elsewhere;
    let peer = ConvKey::Direct(UserId::from_parts("pn", "31000000001").unwrap());
    for kind in [ReceiptType::ReadSelf, ReceiptType::PlayedSelf] {
        let event = own_receipt(
            "31000000001@s.whatsapp.net",
            "99900100123@lid",
            kind,
            &["3EB0AA"],
        );
        assert_eq!(read_elsewhere(&event), Some(peer.clone()));
    }
    let pn = "31000000001@s.whatsapp.net";
    // Not from us, or not a self type, or no message: no chat.
    assert_eq!(
        read_elsewhere(&receipt(pn, pn, ReceiptType::ReadSelf, &["3EB0AA"])),
        None
    );
    assert_eq!(
        read_elsewhere(&receipt(pn, pn, ReceiptType::Read, &["3EB0AA"])),
        None
    );
    assert_eq!(
        read_elsewhere(&own_receipt(pn, pn, ReceiptType::ReadSelf, &[])),
        None
    );
}

fn server_ack(class: Option<&str>, t: Option<i64>, error: Option<&str>) -> Event {
    Event::ServerAck(
        wacore::types::events::ServerAck::builder()
            .id("3EB0E5A7C29F61840237".to_owned())
            .maybe_class(class.map(str::to_owned))
            .maybe_from(Some(
                "15550163928471@s.whatsapp.net"
                    .parse::<Jid>()
                    .expect("a jid"),
            ))
            .maybe_timestamp(t.and_then(|secs| wacore::chrono::DateTime::from_timestamp(secs, 0)))
            .maybe_error(error.map(str::to_owned))
            .build(),
    )
}

/// W5a (brief §2.3): the ack of a MESSAGE stanza is read for its id, its time and its nack code; an ack of any
/// other class (a receipt's ack names a message we RECEIVED) or of no class is not, and an ack is never a signal
/// of the connection nor a durable event. Mutants: the class filter removed; the time dropped; an unreadable code
/// read as an ack.
#[test]
fn a_message_ack_is_read_and_the_acks_of_every_other_class_are_not() {
    use super::message_ack;
    use crate::send::acks::{Answer, Nack};
    let acked = server_ack(Some("message"), Some(1_760_000_456), None);
    let read = message_ack(&acked).expect("a message ack");
    assert_eq!(read.id, "3EB0E5A7C29F61840237");
    assert_eq!(
        read.answer,
        Answer::Acked {
            server_t: Some(1_760_000_456)
        }
    );
    assert_eq!(
        message_ack(&server_ack(Some("message"), None, None)).map(|a| a.answer),
        Some(Answer::Acked { server_t: None })
    );
    assert_eq!(
        message_ack(&server_ack(Some("message"), Some(1), Some("463"))).map(|a| a.answer),
        Some(Answer::Nacked(Nack::Code(463)))
    );
    assert_eq!(
        message_ack(&server_ack(Some("message"), None, Some("x9"))).map(|a| a.answer),
        Some(Answer::Nacked(Nack::Unreadable)),
        "a nack whose code is no number is still a nack"
    );
    for class in [Some("receipt"), Some("notification"), Some("call"), None] {
        assert!(
            message_ack(&server_ack(class, Some(1), None)).is_none(),
            "an ack of class {class:?} must not answer a send"
        );
    }
    assert!(
        translate(&acked, NOW).is_none(),
        "an ack is no connection signal"
    );
    assert!(durable(&acked).is_none(), "an ack is no durable event");
}

/// W5a, brief §2.4: every receipt type that changes no message gives nothing ON PURPOSE, with its reason for the
/// handler's log line: `sent` (a feature-incapable LID peer) is NOT a delivery, the row stays Sent; `sender`,
/// `inactive`, `server-error` and the protocol types likewise. The mapped types give no reason. Mutant: `sent` mapped
/// to Delivered.
#[test]
fn sent_sender_inactive_give_nothing_on_purpose() {
    let pn = "31000000001@s.whatsapp.net";
    for (kind, why) in [
        (ReceiptType::Sent, Unmapped::NotDeliverable),
        (ReceiptType::Sender, Unmapped::Sender),
        (ReceiptType::Inactive, Unmapped::Inactive),
        (ReceiptType::ServerError, Unmapped::ServerError),
        (ReceiptType::Retry, Unmapped::Retry),
        (ReceiptType::EncRekeyRetry, Unmapped::Retry),
        (ReceiptType::PeerMsg, Unmapped::OwnTraffic),
        (ReceiptType::HistorySync, Unmapped::OwnTraffic),
        (ReceiptType::Other("synthetic".into()), Unmapped::Other),
    ] {
        let event = receipt(pn, pn, kind, &["3EB0AA", "3EB0AB"]);
        assert!(
            durable(&event).is_none(),
            "a receipt type that changes no message gave events"
        );
        assert_eq!(unmapped_receipt(&event), Some((why, 2)));
    }
    for kind in [
        ReceiptType::Delivered,
        ReceiptType::Read,
        ReceiptType::Played,
        ReceiptType::ReadSelf,
        ReceiptType::PlayedSelf,
    ] {
        assert_eq!(unmapped_receipt(&receipt(pn, pn, kind, &["3EB0AA"])), None);
    }
}

// ---- W6a2, B6: the server's `groups_dirty` ----

/// A raw `w:gp2` notification whose first child is `first`, holding `groups` as `<group jid>` children.
fn gp2(first: &'static str, groups: &[&str]) -> Event {
    let children: Vec<Node> = groups
        .iter()
        .map(|group| {
            NodeBuilder::new("group")
                .attr("jid", group.parse::<Jid>().expect("a jid"))
                .build()
        })
        .collect();
    let node = NodeBuilder::new("notification")
        .attr("type", "w:gp2")
        .attr("from", "g.us")
        .attr("id", "3178000001")
        .children([NodeBuilder::new(first).children(children).build()])
        .build();
    let packed = wacore_binary::marshal(&node).expect("marshal");
    let bytes = wacore_binary::util::unpack(&packed)
        .expect("unpack")
        .into_owned();
    Event::Notification(std::sync::Arc::new(
        wacore_binary::OwnedNodeRef::new(bytes::Bytes::from(bytes)).expect("a node"),
    ))
}

/// T9: `groups_dirty` reaches the handler only as the raw notification, which also follows every ordinary `w:gp2` stanza.
/// The groups it names are taken (each once, in order, only groups); an ordinary notification names none.
/// Mutants: every notification's first child is read as `groups_dirty`; a group named twice is returned twice.
#[test]
fn a_groups_dirty_names_its_groups_once_and_an_ordinary_notice_names_none() {
    let dirty = gp2(
        "groups_dirty",
        &[
            "120363000071529611@g.us",
            "120363000071529622@g.us",
            "120363000071529611@g.us",
            "15550100177@s.whatsapp.net",
        ],
    );
    assert_eq!(
        dirty_groups(&dirty),
        [
            ConvKey::Group("wag:120363000071529611".into()),
            ConvKey::Group("wag:120363000071529622".into())
        ]
    );
    let ordinary = gp2("promote", &["120363000071529611@g.us"]);
    assert!(dirty_groups(&ordinary).is_empty());
    assert!(dirty_groups(&Event::Connected(Connected::builder().build())).is_empty());
}
