//! Upstream events -> Kourier's own [`WaSignal`] (W2, chunk C1a).
//!
//! This is the ONE file of the W2 state machine that names whatsapp-rust's event types: a pin bump
//! touches it and its tests (`translate::tests`) show what moved. Everything after it (`states`,
//! `link`, the actor) works on [`WaSignal`], which carries only what Kourier decides on, as plain
//! values, so no upstream type crosses into the state machine.
//!
//! The function is PURE: no I/O, no clock (the caller gives `now_ms`), no logging. What it keeps
//! and what it drops, on purpose:
//!
//! - A value that is a secret (QR contents, the pair code, the numbers of a pairing) is a
//!   [`Redacted`], whose `Debug` prints nothing. Nothing else in a signal can name a person.
//! - A text WhatsApp sent for the user (a ban's `message`, a logout's header and subtext) is kept as
//!   plain text, cut and stripped of control characters ([`explanation`]); it is shown, never
//!   logged. A link WhatsApp sent is kept only when it is a bounded `https://` URL ([`https_url`]).
//! - A free-form value the server picks (`violation_reason`, `enforcement_type`) is kept only as a
//!   log-safe token ([`safe_token`]) so that the first real run learns the value set without a
//!   log line that can hold anything else.
//! - A stanza (`raw`) is read for its tag, its `code`, its `conflict` child and one attribute, and
//!   never kept: upstream's own warnings print whole stanzas, Kourier's never do.
//! - The events that are not about the link or the connection (messages, receipts, presence, the
//!   app-state updates, calls, ...) give `None` here; W3a and later steps translate them.
//!
//! Chunk C1a only translates. What a signal means for the connection state and the login is C1c
//! (`states`) and C3 (`link`).

#![allow(
    dead_code,
    reason = "the state machine (states.rs, link.rs) and the actor that use these arrive in C1c, C3 and C4"
)]

use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, MsgTarget, ReceiptKind, Redacted, TargetAuthor, UserId};
use wacore::appstate::patch_decode::WAPatchName;
use wacore::pair_code::PairCodeRejection;
use wacore::types::events::{
    ChatPresenceUpdate, ConnectFailureReason, Event, InboundMessage, LoggedOut as UpLoggedOut,
    ReachoutTimelock, Receipt, ServerAck, TempBanReason,
};
use wacore::types::message::AddressingMode;
use wacore::types::presence::{ChatActivity, ReceiptType};
use wacore_binary::{Node, Server};
use whatsapp_rust::{AppStateError, AppStateResyncReport};

use crate::appstate::ResyncReport;
use crate::inbox::{Arrival, InboxMeta, Origin};
use crate::map::{self, OwnIds};
use crate::notice;

/// The collections upstream knows by the stored names of the dirty rows. A name upstream does not
/// know is left out (`WAPatchName::Unknown` is a parse fallback that a resync refuses whole).
pub(crate) fn collections_of(names: &[String]) -> Vec<WAPatchName> {
    names
        .iter()
        .filter_map(|name| name.parse::<WAPatchName>().ok())
        .filter(|name| *name != WAPatchName::Unknown)
        .collect()
}

/// What a resync report says, by the names the store keeps the collections under: the synced ones and the ones the
/// server refused outright (400/404). The others (retryable, skipped) are neither: they stay unsynced.
pub(crate) fn report_of(report: &AppStateResyncReport) -> ResyncReport {
    let names = |list: &[WAPatchName]| list.iter().map(|name| name.as_str().to_owned()).collect();
    ResyncReport {
        synced: names(&report.synced),
        fatal: names(&report.fatal),
    }
}

/// How a start-up resync call ended, in Kourier's words (INTEROP-9). `answered` is what the call gave, `None` when the
/// task that ran it died (or the client was gone). Nothing of an error is formatted (L5: the kind only).
///
/// - a report: as [`report_of`];
/// - `AppStateError::Internal` (the request WENT OUT and failed: it timed out, or a response could not be read): the
///   collection comes back unsynced, with its flag and owed mark standing, and the run goes on to the next collection;
/// - `NotConnected`, `InvalidRequest` and a dead task (NOTHING was asked): `Err`, the run ends and the rest waits for
///   the next `Online`.
pub(crate) fn resync_answer(
    answered: Option<Result<AppStateResyncReport, AppStateError>>,
) -> Result<ResyncReport, kourier_model::BackendError> {
    match answered {
        Some(Ok(report)) => Ok(report_of(&report)),
        Some(Err(AppStateError::Internal(_))) => {
            tracing::warn!("the app-state resync was asked and came back unsynced");
            Ok(ResyncReport::default())
        }
        Some(Err(_)) | None => Err(kourier_model::BackendError::transient(
            "the app-state resync did not reach the server",
        )),
    }
}

/// What the state machine hears from the client. Built only by [`translate`] (and, in tests, by
/// hand). `Debug` prints no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaSignal {
    // ----- the connection -----
    /// The session is authenticated (upstream's `Connected`). Upstream's fallback note for a
    /// version that could not be fetched is ignored: on a native client the fetch is fatal, so the
    /// note never appears (critic H2).
    Connected,
    /// An established connection ended. Upstream reconnects by itself unless a terminal verdict
    /// follows; `clean` is its own "routine stream recycle" judgment (a log level, nothing more).
    Disconnected { clean: bool },
    /// A failure at connect that upstream keeps retrying by itself, or a rate limit on the stream.
    WillRetry(RetryWhy),
    /// A `<failure>` at connect that is no logout, no ban and not "outdated", and that upstream does
    /// not retry (400, 409, 413, 414, 415, 418, 501, an unknown code, a 402 whose code or time it
    /// could not read): the client stopped for good. `code` is the wire code.
    Refused { code: i32 },
    /// The server refused this build as outdated (`<failure reason="405">`): terminal.
    Outdated,
    /// Another session took this one over (`conflict replaced`, 409): terminal.
    Replaced,
    /// A temporary ban (402): terminal for the client, with the time it lasts.
    Banned(Banned),
    /// The device was logged out by the server (the shapes are [`LogoutShape`]).
    LoggedOut(Logout),
    /// `LoggedOut` that carries no stanza: a logout Kourier asked for itself (critic H8,
    /// `Client::logout` dispatches it before its shutdown). The actor ignores it.
    LocalLogout,
    /// The reach-out lock changed (upstream's `ReachoutTimelockUpdate`, a push). Only a statement
    /// ([`HoldState::Active`] or [`HoldState::Lifted`]); an update that asserts nothing gives no
    /// signal at all.
    Hold(HoldState),
    /// A write the account cannot go on without failed: the hook's inbox commit or the handler's
    /// event write (decision D5). Not an upstream event: the hook and the handler send it themselves,
    /// right before they end the client (`Client::signal_shutdown_sync`, terminal). The actor makes a
    /// NEW client after a backoff.
    WriteFailed,

    // ----- the offline drain (W3a, chunk C7) -----
    /// The server announced a backlog (`<ib><offline_preview>`): a drain begins on this connection.
    /// Its `total` counts stanzas of every kind (receipts, notifications, ...), so it is no unit
    /// for a progress fraction; it is kept to tell a drain that timed out (fix F7).
    OfflinePreview { total: i32 },
    /// Upstream's `OfflineSyncCompleted`. NOT a proof that the drain is durable: upstream also
    /// dispatches it after a failed or held-back tail commit and when its own 60 s wait ran out
    /// (decision D5). The catch-up machine takes Kourier's own boundary at this signal. `count` is
    /// the server's number of stanzas when the end marker came, and what upstream had PROCESSED when
    /// its own wait ran out: below the preview's `total` means the wait ran out (fix F7).
    OfflineDone { count: i32 },
    /// Upstream's `OfflineSyncInterrupted`: the connection ended before the drain did. Nothing is
    /// lost (what was not acknowledged comes again on the next connection), but the account is not
    /// caught up.
    OfflineInterrupted,

    // ----- linking -----
    /// A QR code to show (rotates: 60 s the first time, then 20 s).
    QrCode {
        code: Redacted<String>,
        valid_for: Duration,
    },
    /// The server's refs for QR codes are used up. `socket_closed` is upstream's: the client closed
    /// the connection itself (no pair-code flow was outstanding).
    QrExhausted { socket_closed: bool },
    /// A phone scanned the QR code but runs a WhatsApp without multi-device.
    QrScannedWithoutMultidevice,
    /// The pair code to type on the phone (valid for about three minutes).
    PairCode {
        code: Redacted<String>,
        valid_for: Duration,
    },
    /// The outstanding pair code must be replaced (it is already invalid). `force_manual` is the
    /// server's: the person must ask again.
    PairCodeRefresh { force_manual: bool },
    /// A pair-code request failed. `refusal` is the server's answer when there was one (`None`: the
    /// request never got an answer, or failed local validation).
    PairCodeRefused {
        refusal: Option<PairRefusal>,
        backoff: Option<Duration>,
    },
    /// The phone accepted the link. The two ids are what the identity checks run on.
    PairSuccess(Paired),
    /// A pairing the pre-pair claim (`prepair`, W2c) refused BEFORE the device existed. Kourier's own
    /// signal, not an upstream event: the claim sends it itself, after the error iq left (and past a
    /// gate that is refused, which drops every other signal). The ids are the refused child's; `None`
    /// when it named no device. The login ends with the refusal's own failure; nothing was paired, so
    /// there is nothing to log out.
    PairClaimed(Option<Paired>),
    /// The pairing's cryptography failed.
    PairFailed,
    /// The server asks for a passkey assertion (feature `passkey`). The challenge is not kept: W2
    /// has no authenticator and refuses.
    PasskeyRequested,
    /// The passkey link reached its confirmation stage; `code` is the code to show.
    PasskeyConfirmation { code: Redacted<String> },
    /// The passkey link failed; `continuation` says it failed after the first stage.
    PasskeyFailed { continuation: bool },

    // ----- the chats (W3b, chunk C6a) -----
    /// Someone is typing in a chat, or stopped (upstream's `ChatPresence`). EPHEMERAL: the actor
    /// forwards it as an ephemeral envelope and it never reaches `core_outbox`.
    Typing(Typing),
}

/// Typing in a chat: who, where and whether it goes on. "Recording a voice message" is typing too
/// (decision D3, his "recording shown as typing") until the contract has a kind for it. `Debug` is the
/// model's log-safe form of the ids (a phone number prints no digit); nothing logs a signal anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Typing {
    /// The chat: a person's (`pn:`/`lid:`) or a group's (`wag:`).
    pub conv: ConvKey,
    /// Who types: the chat's peer, or the group member.
    pub by: UserId,
    /// The device, when the id names one.
    pub device: Option<u32>,
    /// Typing (or recording), as opposed to stopped.
    pub on: bool,
}

/// Why a failure is retried by upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryWhy {
    /// `<failure reason="500">`.
    ServerError,
    /// `<failure reason="503">`.
    Unavailable,
    /// `<stream:error code="429">`: upstream waits longer before it reconnects.
    RateLimited,
    /// A connect attempt failed before any stream existed: the transport could not be opened (or
    /// upstream's timeout cancelled the open), or the version file could not be fetched. Not an
    /// upstream event: upstream publishes nothing for it (critic H2); the session reads it where
    /// Kourier owns the seam (`session::client`).
    Unreachable,
}

/// The ban codes (upstream's `TempBanReason`, whatsmeow's names), in Kourier's words. The wire has
/// no 105. The names match K-A's `BanReason`, so the swap in C6b is mechanical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaBan {
    /// 101: messages to too many people who do not have the account in their address books.
    TooManyStrangers,
    /// 102: too many people blocked the account.
    ManyBlocked,
    /// 103: too many groups created with people who do not have the account in their books.
    TooManyGroups,
    /// 104: the same message to too many people.
    SameMessageToMany,
    /// 106: too many messages to a broadcast list.
    BroadcastList,
    /// A code upstream does not know (kept so a server-side addition is visible).
    Other(i32),
}

impl WaBan {
    /// The ban of a wire code.
    pub(crate) fn from_code(code: i32) -> Self {
        match code {
            101 => Self::TooManyStrangers,
            102 => Self::ManyBlocked,
            103 => Self::TooManyGroups,
            104 => Self::SameMessageToMany,
            106 => Self::BroadcastList,
            other => Self::Other(other),
        }
    }

    /// The wire code.
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::TooManyStrangers => 101,
            Self::ManyBlocked => 102,
            Self::TooManyGroups => 103,
            Self::SameMessageToMany => 104,
            Self::BroadcastList => 106,
            Self::Other(code) => code,
        }
    }
}

/// A temporary ban.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Banned {
    pub ban: WaBan,
    /// When it ends: the time the ban was heard plus the duration the server states (the wire's
    /// `expire` is a DURATION in seconds, not a deadline).
    pub until_ms: u64,
    /// WhatsApp's own sentence, for display only.
    pub explanation: Option<String>,
    /// WhatsApp's page about the ban, only as a bounded `https://` URL.
    pub appeal_url: Option<String>,
}

/// A logout by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Logout {
    pub shape: LogoutShape,
    /// The lock's `violation_reason`, as a log-safe token (`other_harm` is the one value seen in
    /// upstream's tests; the set is not closed).
    pub violation: Option<String>,
    /// WhatsApp's header and subtext, for display only.
    pub explanation: Option<String>,
}

/// What the server's logout looked like. The CAUSE is decided from this in `states` (decision L5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogoutShape {
    /// `<conflict type="device_removed">`, or a stream error with code 516: the device was removed
    /// from the phone's list.
    RemovedByPhone,
    /// 401, as a stream error or as a `<failure>`: removed while offline, or unused for too long;
    /// the two are not told apart.
    Unauthorized,
    /// `<failure reason="403">`: upstream's "account/device locked server-side".
    Locked,
    /// `<failure reason="406">`: whatsmeow's "BANNED".
    Refused406,
    /// A logout whose stanza is none of these.
    Other,
}

/// The state of the reach-out lock, as a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HoldState {
    /// `is_active == Some(true)`. `until_ms` is the end the server states (epoch seconds as a
    /// string on the wire), absent when it is missing or not a number.
    Active {
        until_ms: Option<u64>,
        /// `enforcement_type`, as a log-safe token.
        kind: Option<String>,
    },
    /// `is_active == Some(false)`: lifted.
    Lifted,
    /// `is_active == None`: the server asserts nothing.
    Unasserted,
}

/// The server's answer to a pair-code request (`PairCodeRejection`), by its wire code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairRefusal {
    /// 400: malformed, OR throttled for this number (the server reuses `bad-request` for its
    /// per-number limit, so the two cannot be told apart).
    BadRequest,
    /// 403.
    Forbidden,
    /// 429: too many requests on this connection.
    RateOverlimit,
    /// 452: phone-number linking is not enabled for this account; the QR code is the way.
    NotAvailable,
    /// 500.
    ServerError,
    /// Any other code.
    Other(i32),
}

impl PairRefusal {
    /// The refusal of a wire code.
    pub(crate) fn from_code(code: i32) -> Self {
        match code {
            400 => Self::BadRequest,
            403 => Self::Forbidden,
            429 => Self::RateOverlimit,
            452 => Self::NotAvailable,
            500 => Self::ServerError,
            other => Self::Other(other),
        }
    }

    /// Upstream's `is_throttled`: a 429 or a 400 (see [`PairRefusal::BadRequest`]).
    pub(crate) fn is_throttle(self) -> bool {
        matches!(self, Self::BadRequest | Self::RateOverlimit)
    }
}

/// The two ids of a finished pairing. Secrets: `Debug` prints nothing of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Paired {
    /// The phone-number user (digits), upstream's `id.user`.
    pub pn: Redacted<String>,
    /// The LID user, upstream's `lid.user`.
    pub lid: Redacted<String>,
    /// This companion's device number, upstream's `id.device`.
    pub device: u16,
}

/// The signal of one upstream event, or `None` for an event W2 does not act on. `now_ms` is the
/// wall clock in ms, used only to turn a ban's duration into the time it ends.
pub(crate) fn translate(event: &Event, now_ms: u64) -> Option<WaSignal> {
    Some(match event {
        Event::Connected(_) => WaSignal::Connected,
        Event::Disconnected(d) => WaSignal::Disconnected {
            clean: d.reason.is_clean_shutdown(),
        },
        Event::ConnectFailure(f) => {
            if f.reason.should_reconnect() {
                WaSignal::WillRetry(match f.reason {
                    ConnectFailureReason::ServiceUnavailable => RetryWhy::Unavailable,
                    _ => RetryWhy::ServerError,
                })
            } else if f.reason == ConnectFailureReason::ClientOutdated {
                // Upstream dispatches 405 as `ClientOutdated`; this keeps a `ConnectFailure` that
                // somehow carries it from being a mere "refused".
                WaSignal::Outdated
            } else {
                WaSignal::Refused {
                    code: f.reason.code(),
                }
            }
        }
        Event::OfflineSyncPreview(p) => WaSignal::OfflinePreview { total: p.total },
        Event::OfflineSyncCompleted(c) => WaSignal::OfflineDone { count: c.count },
        Event::OfflineSyncInterrupted(_) => WaSignal::OfflineInterrupted,
        Event::ClientOutdated(_) => WaSignal::Outdated,
        Event::StreamReplaced(_) => WaSignal::Replaced,
        Event::StreamError(e) => {
            // Only the 429 means something (node_io: every other stream error without a conflict
            // is informational, a routing failure of one stanza).
            if e.code == "429" {
                WaSignal::WillRetry(RetryWhy::RateLimited)
            } else {
                return None;
            }
        }
        Event::TemporaryBan(b) => WaSignal::Banned(Banned {
            ban: WaBan::from_code(TempBanReason::code(&b.code)),
            until_ms: now_ms.saturating_add(duration_ms(&b.expire)),
            explanation: b.message.as_deref().and_then(explanation),
            appeal_url: b.url.as_deref().and_then(https_url),
        }),
        Event::LoggedOut(l) => logged_out(l),
        Event::ReachoutTimelockUpdate(u) => match hold_of(&u.state) {
            HoldState::Unasserted => return None,
            hold => WaSignal::Hold(hold),
        },

        Event::PairingQrCode(q) => WaSignal::QrCode {
            code: Redacted::new(q.code.clone()),
            valid_for: q.timeout,
        },
        Event::PairingQrCodesExhausted(e) => WaSignal::QrExhausted {
            socket_closed: e.disconnected,
        },
        Event::QrScannedWithoutMultidevice(_) => WaSignal::QrScannedWithoutMultidevice,
        Event::PairingCode(c) => WaSignal::PairCode {
            code: Redacted::new(c.code.clone()),
            valid_for: c.timeout,
        },
        Event::PairingCodeRefresh(r) => WaSignal::PairCodeRefresh {
            force_manual: r.force_manual,
        },
        Event::PairingCodeError(e) => WaSignal::PairCodeRefused {
            refusal: e
                .rejection
                .map(|r| PairRefusal::from_code(PairCodeRejection::code(&r))),
            backoff: e.backoff,
        },
        Event::PairSuccess(p) => WaSignal::PairSuccess(Paired {
            pn: Redacted::new(p.id.user.to_string()),
            lid: Redacted::new(p.lid.user.to_string()),
            device: p.id.device,
        }),
        Event::PairError(_) => WaSignal::PairFailed,
        Event::PairPasskeyRequest(_) => WaSignal::PasskeyRequested,
        Event::PairPasskeyConfirmation(c) => WaSignal::PasskeyConfirmation {
            code: Redacted::new(c.code.clone()),
        },
        Event::PairPasskeyError(e) => WaSignal::PasskeyFailed {
            continuation: e.continuation,
        },

        Event::ChatPresence(update) => WaSignal::Typing(typing_of(update)?),

        _ => return None,
    })
}

/// A chat state as typing. `Typing` and `RecordingAudio` are "on" (decision D3: a voice message being
/// recorded reads as typing until the contract can say so), `Idle` and any state a later upstream adds
/// are "off". `None` when the chat is no conversation of the contract's (a status, a list) or the
/// sender is no person.
fn typing_of(update: &ChatPresenceUpdate) -> Option<Typing> {
    let conv = map::conv_of(&update.source.chat)?;
    let sender = &update.source.sender;
    let by = map::person_of(sender)?;
    Some(Typing {
        conv,
        by,
        device: (sender.device > 0).then_some(u32::from(sender.device)),
        on: matches!(
            update.state,
            ChatActivity::Typing | ChatActivity::RecordingAudio
        ),
    })
}

/// The state of the reach-out lock a [`ReachoutTimelock`] states. The push (`translate`) and the
/// pull after a connect (`fetch_reachout_timelock`) both read it here, so they cannot disagree.
pub(crate) fn hold_of(state: &ReachoutTimelock) -> HoldState {
    match state.is_active {
        Some(true) => HoldState::Active {
            until_ms: state
                .time_enforcement_ends
                .as_deref()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .and_then(|secs| secs.checked_mul(1000)),
            kind: state.enforcement_type.as_deref().and_then(safe_token),
        },
        Some(false) => HoldState::Lifted,
        None => HoldState::Unasserted,
    }
}

/// A logout with a stanza is a server logout; one without is Kourier's own (critic H8).
fn logged_out(l: &UpLoggedOut) -> WaSignal {
    let Some(raw) = l.raw.as_ref() else {
        return WaSignal::LocalLogout;
    };
    let shape = match raw.tag.as_ref() {
        "stream:error" => {
            let device_removed = child(raw, "conflict")
                .and_then(|c| attr(c, "type"))
                .is_some_and(|t| t == "device_removed");
            match attr(raw, "code").as_deref() {
                _ if device_removed => LogoutShape::RemovedByPhone,
                Some("516") => LogoutShape::RemovedByPhone,
                Some("401") => LogoutShape::Unauthorized,
                _ => LogoutShape::Other,
            }
        }
        "failure" => match l.reason {
            ConnectFailureReason::LoggedOut => LogoutShape::Unauthorized,
            ConnectFailureReason::AccountLocked => LogoutShape::Locked,
            ConnectFailureReason::UnknownLogout => LogoutShape::Refused406,
            _ => LogoutShape::Other,
        },
        _ => LogoutShape::Other,
    };
    let explanation = l.logout_message.as_ref().and_then(|m| {
        let text = [m.header.as_deref(), m.subtext.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        explanation(&text)
    });
    WaSignal::LoggedOut(Logout {
        shape,
        violation: attr(raw, "violation_reason")
            .as_deref()
            .and_then(safe_token),
        explanation,
    })
}

/// An attribute of a stanza as text (`None` when it is absent).
fn attr(node: &Node, key: &str) -> Option<String> {
    node.attrs.get(key).map(|v| v.as_str().into_owned())
}

/// The first child of a stanza with this tag.
fn child<'a>(node: &'a Node, tag: &str) -> Option<&'a Node> {
    node.get_optional_child(tag)
}

/// A duration in whole milliseconds, never negative.
fn duration_ms(d: &wacore::chrono::Duration) -> u64 {
    u64::try_from(d.num_milliseconds()).unwrap_or(0)
}

/// The longest explanation Kourier keeps, in characters.
pub(crate) const EXPLANATION_MAX: usize = 400;

/// The longest link Kourier keeps, in bytes.
pub(crate) const URL_MAX: usize = 2048;

/// WhatsApp's own text for the user: control characters (newlines included) and white space
/// become one space each run, the bidirectional controls that can reorder what follows are
/// dropped, the ends are trimmed and the whole is cut at [`EXPLANATION_MAX`] characters. Empty
/// gives `None`. It is plain text: never markup, never logged.
pub(crate) fn explanation(text: &str) -> Option<String> {
    let cleaned: String = text.chars().filter(|c| !is_bidi_control(*c)).collect();
    let collapsed = cleaned
        .split(|c: char| c.is_control() || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = collapsed.chars().take(EXPLANATION_MAX).collect();
    let cut = cut.trim_end();
    (!cut.is_empty()).then(|| cut.to_owned())
}

/// The marks that reorder or isolate text (U+061C, U+200E, U+200F, U+202A-202E, U+2066-2069).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// A link WhatsApp sent, kept only as `https://<host>...` with no white space or control
/// character, at most [`URL_MAX`] bytes. Anything else gives `None`; the UI opens it only after a
/// click and only through the application's one opener.
pub(crate) fn https_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let ok = url.len() <= URL_MAX
        && !host.is_empty()
        && !host.contains(['@', '\\'])
        && !url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || is_bidi_control(c));
    ok.then(|| url.to_owned())
}

/// A server-chosen value as a token that is safe to log: ASCII letters, digits, `_`, `-` and `.`,
/// at most 48 characters, no run of six digits or more (a number), lower-cased. Anything else gives
/// `None`: the value is then not logged at all.
pub(crate) fn safe_token(value: &str) -> Option<String> {
    let ok = !value.is_empty()
        && value.len() <= 48
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !has_digit_run(value, 6);
    ok.then(|| value.to_ascii_lowercase())
}

/// Whether `text` holds `n` or more digits in a row.
fn has_digit_run(text: &str, n: usize) -> bool {
    let mut run = 0usize;
    for c in text.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if run >= n {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// The events of `event` that must be committed to `core_outbox` BEFORE the handler returns, because
/// upstream acknowledges the stanza to the server right after (architecture §7.1 rule (a), decision
/// D1): the caller writes them with `events::EventWriter::write_and_wait` and, when that fails, ends the
/// connection instead of letting the ack go out. Pure. `None` for every event the rule does not
/// cover (the connection and the login are decided by [`translate`]; the messages of the hook are in
/// the inbox; typing is ephemeral and goes through the actor). `now_ms` is the local clock (the
/// `received_at` of a message made here) and `own` the account's own ids (an act of our own is `by: None`).
///
/// Three kinds: a delivery, read or played receipt for a message of ours (W3a; the pair of ids the stanza
/// vouches for goes first, W3b fix F8), our own read or view
/// of somebody's message on another device (W3b, [`receipts`]) and a message that could not be decrypted
/// ([`notice::undecryptable`]). A group's notification is the fourth kind of the rule, and [`noticed`] makes it (it needs
/// the group's row). Retries, sender fan-outs, history
/// syncs, the status feed and the rest give `None`; W5 reads `Sent` and `Sender` for its send states.
pub(crate) fn durable(event: &Event, now_ms: u64, own: &OwnIds) -> Option<Vec<BackendEvent>> {
    match event {
        Event::Receipt(receipt) => receipts(receipt, own),
        Event::UndecryptableMessage(lost) => notice::undecryptable(lost, now_ms),
        _ => None,
    }
}

/// A group's notification as the handler writes it (W6a2, B5): the events that need no row and the acts that need the
/// group's row, made into events in the zone that commits them (`events::EventWriter::write_notice_and_wait`). Pure.
/// `None` for every other event and for an action this build leaves out. The same rule (a) as [`durable`]: upstream
/// acknowledges the stanza right after the handler returns.
pub(crate) fn noticed(event: &Event, now_ms: u64, own: &OwnIds) -> Option<notice::Noticed> {
    match event {
        Event::GroupUpdate(update) => notice::group_notice(update, now_ms, own),
        _ => None,
    }
}

/// The groups a server's `groups_dirty` names (W6a2, B6; scout T9): it reaches the handler ONLY as the raw
/// `Event::Notification`, which also follows every ordinary `w:gp2` stanza, so a notification is taken only when its first
/// child is `groups_dirty` (upstream's own parse says so). Each group once, in the stanza's order; an id that is no group
/// is left out. Pure. The stanza asks nothing of upstream (it only drops its own cache): what Kourier does with the
/// names is a "fetch me" mark under the fetch policy's bucket.
pub(crate) fn dirty_groups(event: &Event) -> Vec<ConvKey> {
    let Event::Notification(node) = event else {
        return Vec::new();
    };
    let Some(named) = wacore::stanza::groups::parse_groups_dirty(node.get()) else {
        return Vec::new();
    };
    let mut groups: Vec<ConvKey> = Vec::new();
    for conv in named.iter().filter_map(crate::groups::group_conv) {
        if !groups.contains(&conv) {
            groups.push(conv);
        }
    }
    groups
}

/// Whether `event` is the dispatch of an app-state mutation (or the report of a sync that left collections unsynced):
/// the traffic rule-(b)'s quiet look counts (W6a1, D4). Presence, typing, receipts, messages and every other event are
/// not; neither is a sent frame. A pin bump that adds a mutation event adds it here, or its collection can be cleared
/// while the mutations still arrive (the quiet look is the safe side only for what it counts).
pub(crate) fn is_app_state(event: &Event) -> bool {
    matches!(
        event,
        Event::ArchiveUpdate(_)
            | Event::PinUpdate(_)
            | Event::MuteUpdate(_)
            | Event::StarUpdate(_)
            | Event::MarkChatAsReadUpdate(_)
            | Event::DeleteChatUpdate(_)
            | Event::ClearChatUpdate(_)
            | Event::DeleteMessageForMeUpdate(_)
            | Event::UserStatusMuteUpdate(_)
            | Event::LockChatUpdate(_)
            | Event::LabelEditUpdate(_)
            | Event::LabelAssociationUpdate(_)
            | Event::LabelSublistUpdate(_)
            | Event::MessageLabelAssociationUpdate(_)
            | Event::ContactUpdate(_)
            | Event::ContactRemoved(_)
            | Event::SelfPushNameUpdated(_)
            | Event::QuickReplyUpdate(_)
            | Event::DisableLinkPreviewsUpdate(_)
            | Event::UnarchiveChatsSettingUpdate(_)
            | Event::FavoriteStickerUpdate(_)
            | Event::RemoveRecentStickerUpdate(_)
            | Event::FavoritesUpdate(_)
            | Event::StatusPrivacyUpdate(_)
            | Event::CallLogSync(_)
            | Event::AppStateSyncFailed(_)
    )
}

/// The server's ack of a MESSAGE stanza (W5a, brief §2.3): `Event::ServerAck` of class `message`, as the ack
/// registry takes it (`send::acks`). Acks of every other class (receipts, notifications, calls; a receipt's ack
/// carries the id of a RECEIVED message) and an ack without a class give `None`. The `t` is kept in seconds when
/// it was readable, the `error` as its number or as unreadable; the `from` (the chat) is not read: an ack is
/// correlated by its id alone, as upstream does. Pure, like everything here.
pub(crate) fn message_ack(event: &Event) -> Option<crate::send::acks::Ack> {
    use crate::send::acks::{Ack, Answer, Nack};
    let Event::ServerAck(ServerAck {
        id,
        class,
        timestamp,
        error,
        ..
    }) = event
    else {
        return None;
    };
    if class.as_deref() != Some("message") || id.is_empty() {
        return None;
    }
    let answer = match error {
        None => Answer::Acked {
            server_t: timestamp.map(|t| t.timestamp()),
        },
        Some(code) => Answer::Nacked(code.parse::<u16>().map_or(Nack::Unreadable, Nack::Code)),
    };
    Some(Ack {
        id: id.clone(),
        answer,
    })
}

/// The chat an own read or view on another device (`ReadSelf`, `PlayedSelf`, from our own account) was about, for the
/// history's unread hint (W4 fix F7): a chat read on the phone has no unread message any more, whatever an older
/// chunk's count says. `None` for every other event and for a receipt [`receipts`] would not make an own read of.
pub(crate) fn read_elsewhere(event: &Event) -> Option<ConvKey> {
    let Event::Receipt(receipt) = event else {
        return None;
    };
    (matches!(
        receipt.r#type,
        ReceiptType::ReadSelf | ReceiptType::PlayedSelf
    ) && receipt.source.is_from_me
        && !receipt.message_ids.is_empty())
    .then(|| map::conv_of(&receipt.source.chat))
    .flatten()
}

/// Why a receipt type changes no message (W5a, brief §2.4): every type [`receipts`] does not map is named here
/// with its reason, on purpose (a type a later pin adds falls under [`Unmapped::Other`] and is logged as such).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unmapped {
    /// `sent`: the server took the message but the recipient cannot take it (a feature-incapable LID peer, WA
    /// Web's downgrade of a delivery). It is NOT delivered: the row stays `Sent`, never `Delivered`.
    NotDeliverable,
    /// `sender`: our own other device's receipt of a message we sent from there; the core's row knows it is ours.
    Sender,
    /// `retry` / `enc_rekey_retry`: upstream answers a peer's retry (re-encrypts `sent_messages`) or re-keys a call
    /// itself; nothing about the message changes.
    Retry,
    /// `inactive`: a receipt of a device that is not active; it says nothing about the message.
    Inactive,
    /// `server-error`: the server's own error receipt; the send's outcome is the ack's (`send::acks`).
    ServerError,
    /// `peer_msg` / `hist_sync`: our own devices' protocol traffic (upstream's own).
    OwnTraffic,
    /// A type this build does not know.
    Other,
}

impl Unmapped {
    /// The reason as a log field.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::NotDeliverable => "not deliverable",
            Self::Sender => "sender",
            Self::Retry => "retry",
            Self::Inactive => "inactive",
            Self::ServerError => "server error",
            Self::OwnTraffic => "own devices",
            Self::Other => "unknown type",
        }
    }
}

/// The contract's receipt kind of a type and whether it is one of OUR reads (the self types), or why it maps to
/// nothing.
fn kind_of(kind: &ReceiptType) -> Result<(ReceiptKind, bool), Unmapped> {
    Ok(match kind {
        ReceiptType::Delivered => (ReceiptKind::Delivered, false),
        ReceiptType::Read => (ReceiptKind::Read, false),
        ReceiptType::Played => (ReceiptKind::Viewed, false),
        ReceiptType::ReadSelf => (ReceiptKind::Read, true),
        ReceiptType::PlayedSelf => (ReceiptKind::Viewed, true),
        ReceiptType::Sent => return Err(Unmapped::NotDeliverable),
        ReceiptType::Sender => return Err(Unmapped::Sender),
        ReceiptType::Retry | ReceiptType::EncRekeyRetry => return Err(Unmapped::Retry),
        ReceiptType::Inactive => return Err(Unmapped::Inactive),
        ReceiptType::ServerError => return Err(Unmapped::ServerError),
        ReceiptType::PeerMsg | ReceiptType::HistorySync => return Err(Unmapped::OwnTraffic),
        // `Other` and a type a later pin adds (the enum is `non_exhaustive`).
        _ => return Err(Unmapped::Other),
    })
}

/// A receipt whose TYPE maps to nothing, with its reason and how many messages it names, for the handler's log
/// line (counted by kind, never an id). `None` for every other event and for a type [`receipts`] maps (even when
/// that receipt then gives nothing for another reason).
pub(crate) fn unmapped_receipt(event: &Event) -> Option<(Unmapped, usize)> {
    let Event::Receipt(receipt) = event else {
        return None;
    };
    kind_of(&receipt.r#type)
        .err()
        .map(|why| (why, receipt.message_ids.len()))
}

/// `Event::Receipt` as the contract's `Receipts`. WhatsApp receipts name the chat, so a receipt lands
/// only on a message of that conversation, and `by` is the receipt's actor (`source.sender`, not the
/// author of the message).
///
/// - Delivered, read, played of someone else: the targets are OUR messages (`TargetAuthor::Own`).
///   In a group each participant has a receipt of his own, `by` names him.
/// - Our own read or view on another device (`ReadSelf`, `PlayedSelf`): upstream folds a `read` or
///   `played` authored by our own account into these (`src/receipt.rs` `address_receipt`) and re-addresses
///   the chat to the peer's thread. The targets are somebody else's messages of that chat: the
///   author is not stated (`TargetAuthor::Unknown`), and `by` is one of OUR ids, which the contract
///   reads as our own read ("a receipt whose `by` is one of our own ids", `api/event.rs`).
///
/// The id pair the stanza vouches for (`source.sender` with the `sender_alt` upstream read from
/// `participant_pn`/`participant_lid`) goes FIRST as an `IdentityMerged`, by the rules of every other
/// pair (`map::vouched_pair`: one number and one LID, never one of our own ids): after the LID migration
/// a member's receipt may come from his LID while our message went to his number, and the core keeps a
/// receipt for two days waiting for the merge (W3b fix F8).
///
/// A self type from an actor that is not us, or a plain type from our own account, says nothing about
/// us and gives `None`; so do the status feed (no conversation) and a receipt without ids.
fn receipts(receipt: &Receipt, own_ids: &OwnIds) -> Option<Vec<BackendEvent>> {
    let (kind, own) = kind_of(&receipt.r#type).ok()?;
    if receipt.source.is_from_me != own {
        return None;
    }
    let conv = map::conv_of(&receipt.source.chat)?;
    let by = map::person_of(&receipt.source.sender)?;
    let author = if own {
        TargetAuthor::Unknown
    } else {
        TargetAuthor::Own
    };
    let targets: Vec<MsgTarget> = receipt
        .message_ids
        .iter()
        .map(|id| MsgTarget {
            conv: Some(conv.clone()),
            author: author.clone(),
            local: id.to_string().into(),
            alternates: Vec::new(),
        })
        .collect();
    if targets.is_empty() {
        return None;
    }
    let mut events = Vec::new();
    if let Some(alt) = &receipt.source.sender_alt
        && let Some((from, into)) = map::vouched_pair(&receipt.source.sender, alt, own_ids)
    {
        events.push(BackendEvent::IdentityMerged { from, into });
    }
    events.push(BackendEvent::Receipts {
        targets,
        by,
        kind,
        at: receipt.timestamp.timestamp_millis(),
    });
    events
        .iter()
        .all(|event| event.validate_ids().is_ok())
        .then_some(events)
}

/// The messages of an `Event::Messages` that the durability hook did NOT commit (newsletters and
/// messages recovered by PDO: `MessageBatch::hook_committed` is the documented flag), as inbox rows.
/// The caller queues them with `Inbox::commit_detached`, which does not wait: upstream acknowledges
/// those stanzas on its own path before any handler runs, so they are at most once (decision D4). A
/// batch the hook committed gives nothing: the inbox has it. Pure.
pub(crate) fn unhooked(event: &Event) -> Vec<Arrival> {
    let Event::Messages(batch) = event else {
        return Vec::new();
    };
    if batch.hook_committed {
        return Vec::new();
    }
    batch
        .iter()
        .map(|item| arrival(item, unhooked_origin(item)))
        .collect()
}

/// Where an item that did not go through the hook came from, by the documented predicates: a
/// newsletter's chat, else a PDO recovery. `hook_committed == false` is
/// only ever those two producers; anything else is kept as a recovery, which stage 2 maps like a
/// message.
fn unhooked_origin(item: &InboundMessage) -> Origin {
    if item.info.source.chat.server == Server::Newsletter {
        Origin::Newsletter
    } else {
        Origin::Pdo
    }
}

/// The inbox row of one message upstream's durability hook (or, for a newsletter or PDO item, the
/// event handler) hands over: the identity the hook contract dedupes on, the encoded message and the
/// fields stage 2 needs of its `MessageInfo` (see [`InboxMeta`]). Pure; keeps nothing that is not in
/// the row.
pub(crate) fn arrival(item: &InboundMessage, origin: Origin) -> Arrival {
    let info = &item.info;
    Arrival {
        chat: info.source.chat.to_string(),
        sender: info.source.sender.to_string(),
        id: info.id.to_string(),
        origin,
        message: waproto::codec::message_to_vec(&item.message),
        meta: InboxMeta {
            v: InboxMeta::VERSION,
            timestamp: info.timestamp.timestamp(),
            from_me: info.source.is_from_me,
            is_group: info.source.is_group,
            push_name: info.push_name.to_string(),
            lid_addressing: matches!(info.source.addressing_mode, Some(AddressingMode::Lid)),
            sender_alt: info.source.sender_alt.as_ref().map(ToString::to_string),
            recipient_alt: info.source.recipient_alt.as_ref().map(ToString::to_string),
            ephemeral_expiration: item.ephemeral_expiration,
            is_offline: info.is_offline,
        },
    }
}

#[cfg(test)]
mod tests;
