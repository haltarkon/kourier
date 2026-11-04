//! T-5: the table of `states.rs`, row by row, and the rules that hold for every input.

use kourier_model::{
    AccountHold, BanReason, ConnState, HoldReason, LogoutCause, NetworkNote, Restriction,
    UserAction,
};
use wacore::chrono::Duration as ChronoDuration;
use wacore::types::events::{
    ConnectFailureReason, Event, LoggedOut, LogoutMessage, TempBanReason, TemporaryBan,
};
use wacore_binary::builder::NodeBuilder;

use super::*;
use crate::translate::{Paired, translate};

fn logout(shape: LogoutShape, violation: Option<&str>) -> Logout {
    Logout {
        shape,
        violation: violation.map(str::to_owned),
        explanation: None,
    }
}

fn state(signal: WaSignal) -> WaState {
    state_of(&signal).expect("a state")
}

const NOW: u64 = 1_700_000_000_000;

/// What upstream says of a temporary ban, as it goes through `translate` (the one place that names
/// upstream's types), so the table below starts at the wire code.
fn ban_event(code: i32, message: Option<&str>, url: Option<&str>) -> Event {
    Event::TemporaryBan(Box::new(
        TemporaryBan::builder()
            .code(TempBanReason::from(code))
            .expire(ChronoDuration::try_seconds(3_600).unwrap())
            .maybe_message(message.map(str::to_owned))
            .maybe_url(url.map(str::to_owned))
            .build(),
    ))
}

/// The state of a ban event, which must be a `Restricted` one.
fn banned(code: i32, message: Option<&str>, url: Option<&str>) -> WaState {
    let signal = translate(&ban_event(code, message, url), NOW).expect("a ban signal");
    state(signal)
}

#[test]
fn a_connection_that_comes_and_goes_is_syncing_and_offline_and_may_be_tried_again() {
    // `Online` is the catch-up machine's to show (W3a): a connection alone is `Syncing`.
    let online = state(WaSignal::Connected);
    assert_eq!(online.conn, ConnState::Syncing { fraction: None });
    for signal in [
        WaSignal::Disconnected { clean: true },
        WaSignal::Disconnected { clean: false },
        WaSignal::WillRetry(RetryWhy::ServerError),
        WaSignal::WillRetry(RetryWhy::Unavailable),
        WaSignal::WillRetry(RetryWhy::RateLimited),
        WaSignal::WillRetry(RetryWhy::Unreachable),
    ] {
        let s = state(signal.clone());
        assert_eq!(
            s.conn,
            ConnState::Offline { retry_at_ms: None },
            "{signal:?}"
        );
        // Upstream reconnects by itself: nothing stopped, no typed facts.
        assert!(!s.terminal);
    }
    assert!(!online.terminal);
}

#[test]
fn a_verdict_that_stops_the_client_asks_the_person() {
    let outdated = state(WaSignal::Outdated);
    assert_eq!(
        outdated.conn,
        ConnState::NeedsUserAction(UserAction::UpdateApp)
    );
    let replaced = state(WaSignal::Replaced);
    assert_eq!(
        replaced.conn,
        ConnState::NeedsUserAction(UserAction::Reconnect)
    );
    // A refusal at connect upstream does not retry: the person decides, nothing loops.
    let refused = state(WaSignal::Refused { code: 418 });
    assert_eq!(
        refused.conn,
        ConnState::NeedsUserAction(UserAction::Reconnect)
    );
    for s in [outdated, replaced, refused] {
        assert!(s.terminal);
    }
}

#[test]
fn a_ban_restricts_the_account_until_it_ends_and_stops_the_client() {
    let s = state(WaSignal::Banned(Banned {
        ban: WaBan::SameMessageToMany,
        until_ms: 1_700_086_400_000,
        explanation: Some("Too many copies.".to_owned()),
        appeal_url: Some("https://faq.example.invalid/ban".to_owned()),
    }));
    assert_eq!(
        s.conn,
        ConnState::Restricted {
            until_ms: Some(1_700_086_400_000),
            reason: "temporary ban 104".to_owned(),
            kind: Restriction::Banned(BanReason::SameMessageToMany),
            note: NetworkNote {
                explanation: Some("Too many copies.".to_owned()),
                appeal_url: Some("https://faq.example.invalid/ban".to_owned()),
            },
        }
    );
    // The client is stopped: it connects again only when the person asks (Q1).
    assert!(s.terminal);
}

#[test]
fn every_ban_code_has_its_reason() {
    // The table of the module doc, from the wire code through `translate` and `state_of`.
    for (code, reason) in [
        (101, BanReason::TooManyStrangers),
        (102, BanReason::ManyBlocked),
        (103, BanReason::TooManyGroups),
        (104, BanReason::SameMessageToMany),
        (106, BanReason::BroadcastList),
        // The wire has no 105; any other code is kept for the log, the UI says only "banned".
        (105, BanReason::Other(105)),
        (0, BanReason::Other(0)),
        (107, BanReason::Other(107)),
        (4242, BanReason::Other(4242)),
        (-1, BanReason::Other(-1)),
    ] {
        let s = banned(code, None, None);
        assert_eq!(
            s.conn,
            ConnState::Restricted {
                // The server states a duration; the account is restricted until it has passed.
                until_ms: Some(NOW + 3_600_000),
                reason: format!("temporary ban {code}"),
                kind: Restriction::Banned(reason),
                note: NetworkNote::default(),
            },
            "code {code}"
        );
        assert!(s.terminal, "code {code}");
    }
}

#[test]
fn a_ban_carries_whatsapps_sentence_and_page_when_it_has_them_and_nothing_else() {
    let note_of = |s: WaState| match s.conn {
        ConnState::Restricted { note, .. } => note,
        other => panic!("not a ban: {other:?}"),
    };
    // Both, cleaned in `translate`: one line of plain text, the page as it came.
    assert_eq!(
        note_of(banned(
            104,
            Some("  Too many\ncopies.\u{202e} "),
            Some("https://faq.example.invalid/ban")
        )),
        NetworkNote {
            explanation: Some("Too many copies.".to_owned()),
            appeal_url: Some("https://faq.example.invalid/ban".to_owned()),
        }
    );
    // The sentence alone, the page alone.
    assert_eq!(
        note_of(banned(101, Some("Slow down."), None)),
        NetworkNote {
            explanation: Some("Slow down.".to_owned()),
            appeal_url: None,
        }
    );
    assert_eq!(
        note_of(banned(102, None, Some("https://faq.example.invalid/x"))),
        NetworkNote {
            explanation: None,
            appeal_url: Some("https://faq.example.invalid/x".to_owned()),
        }
    );
    // Where it said nothing, or nothing that may be shown: the default note. A page that is not
    // an `https` address never reaches the contract.
    for (message, url) in [
        (None, None),
        (Some("   "), None),
        (None, Some("http://faq.example.invalid/ban")),
        (None, Some("javascript:alert(1)")),
        (None, Some("https://")),
        (None, Some("https://user@faq.example.invalid/ban")),
        (None, Some("https://faq.example.invalid/a b")),
    ] {
        let note = note_of(banned(103, message, url));
        assert_eq!(note, NetworkNote::default(), "{message:?} {url:?}");
        assert!(note.is_empty());
    }
}

#[test]
fn every_logout_shape_has_its_cause() {
    for (shape, violation, cause, reason) in [
        (
            LogoutShape::RemovedByPhone,
            None,
            LogoutCause::RemovedOnPhone,
            "device removed",
        ),
        (
            LogoutShape::Unauthorized,
            None,
            LogoutCause::Unknown,
            "unauthorized (401)",
        ),
        (
            LogoutShape::Locked,
            None,
            LogoutCause::AccountGone,
            "account locked (403)",
        ),
        (
            LogoutShape::Locked,
            Some("other_harm"),
            LogoutCause::AccountGone,
            "account locked (403)",
        ),
        (
            LogoutShape::Locked,
            Some("unofficial_client"),
            LogoutCause::Unofficial,
            "account locked (403)",
        ),
        (
            LogoutShape::Refused406,
            None,
            LogoutCause::AccountGone,
            "account refused (406)",
        ),
        (LogoutShape::Other, None, LogoutCause::Unknown, "logged out"),
    ] {
        let s = state(WaSignal::LoggedOut(logout(shape, violation)));
        // WhatsApp said nothing more for these: the default note.
        assert_eq!(
            s.conn,
            ConnState::LoggedOut {
                reason: reason.to_owned(),
                cause,
                note: NetworkNote::default(),
            },
            "{shape:?} {violation:?}"
        );
        assert!(s.terminal);
    }
}

#[test]
fn a_logout_carries_whatsapps_sentence_and_never_a_page() {
    // Upstream's logout has a header and a subtext and no link: the note has the sentence only.
    for shape in [
        LogoutShape::RemovedByPhone,
        LogoutShape::Unauthorized,
        LogoutShape::Locked,
        LogoutShape::Refused406,
        LogoutShape::Other,
    ] {
        let s = state(WaSignal::LoggedOut(Logout {
            shape,
            violation: None,
            explanation: Some("Account locked.".to_owned()),
        }));
        let ConnState::LoggedOut { note, .. } = s.conn else {
            panic!("not a logout: {shape:?}");
        };
        assert_eq!(
            note,
            NetworkNote {
                explanation: Some("Account locked.".to_owned()),
                appeal_url: None,
            },
            "{shape:?}"
        );
    }
}

#[test]
fn a_logout_from_upstreams_event_keeps_its_cleaned_sentence_and_its_cause() {
    let raw = NodeBuilder::new("failure")
        .attr("reason", "403")
        .attr("violation_reason", "unofficial_client")
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
    let s = state(translate(&event, NOW).expect("a logout signal"));
    assert_eq!(
        s.conn,
        ConnState::LoggedOut {
            reason: "account locked (403)".to_owned(),
            cause: LogoutCause::Unofficial,
            note: NetworkNote {
                explanation: Some("Account locked Open WhatsApp on your phone.".to_owned()),
                appeal_url: None,
            },
        }
    );
}

#[test]
fn the_networks_words_are_not_in_what_a_debug_print_of_the_state_says() {
    // The actor logs `conn` with `{:?}`; the kind and the cause must be there, the text never.
    let ban = banned(
        104,
        Some("Secret sentence."),
        Some("https://faq.example.invalid/secret-page"),
    );
    let said = format!("{ban:?}");
    assert!(said.contains("Banned(SameMessageToMany)"), "{said}");
    let out = state(WaSignal::LoggedOut(Logout {
        shape: LogoutShape::RemovedByPhone,
        violation: None,
        explanation: Some("Secret sentence.".to_owned()),
    }));
    let said_out = format!("{out:?}");
    assert!(said_out.contains("RemovedOnPhone"), "{said_out}");
    for said in [said, said_out] {
        assert!(!said.contains("Secret sentence"), "{said}");
        assert!(!said.contains("secret-page"), "{said}");
    }
}

#[test]
fn phone_inactive_is_never_produced() {
    // No upstream signal tells it today (the brief's 3.5).
    for shape in [
        LogoutShape::RemovedByPhone,
        LogoutShape::Unauthorized,
        LogoutShape::Locked,
        LogoutShape::Refused406,
        LogoutShape::Other,
    ] {
        for violation in [
            None,
            Some("other_harm"),
            Some("unofficial"),
            Some("inactive"),
        ] {
            assert_ne!(
                logout_cause(&logout(shape, violation)),
                LogoutCause::PhoneInactive,
                "{shape:?} {violation:?}"
            );
        }
    }
}

#[test]
fn a_signal_that_is_not_about_the_connection_changes_no_state() {
    let paired = Paired {
        pn: "1".to_owned().into(),
        lid: "2".to_owned().into(),
        device: 3,
    };
    for signal in [
        WaSignal::LocalLogout,
        WaSignal::PairSuccess(paired),
        WaSignal::PairFailed,
        WaSignal::PasskeyRequested,
        WaSignal::QrScannedWithoutMultidevice,
        WaSignal::QrExhausted {
            socket_closed: false,
        },
        WaSignal::Hold(HoldState::Lifted),
    ] {
        assert_eq!(state_of(&signal), None, "{signal:?}");
    }
}

#[test]
fn the_push_and_the_pull_of_the_reach_out_lock_make_their_statements() {
    let active = HoldState::Active {
        until_ms: Some(1_704_153_600_000),
        kind: None,
    };
    let open_ended = HoldState::Active {
        until_ms: None,
        kind: None,
    };
    let hold = |until_ms| AccountHold {
        until_ms,
        reason: HoldReason::ReachOutLocked,
    };
    // The push: a lock, a lock without a time, a lifted lock; an update that asserts nothing says nothing.
    assert_eq!(
        hold_push(&active),
        Some(Some(hold(Some(1_704_153_600_000))))
    );
    assert_eq!(hold_push(&open_ended), Some(Some(hold(None))));
    assert_eq!(hold_push(&HoldState::Lifted), Some(None));
    assert_eq!(hold_push(&HoldState::Unasserted), None);
    // The pull at a connect states exactly one thing: a lock, or no lock. Never "nothing".
    assert_eq!(hold_pull(&active), Some(hold(Some(1_704_153_600_000))));
    assert_eq!(hold_pull(&open_ended), Some(hold(None)));
    assert_eq!(hold_pull(&HoldState::Lifted), None);
    assert_eq!(hold_pull(&HoldState::Unasserted), None);
}

/// A small deterministic generator (no dependency): the tests must repeat.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[(self.next() as usize) % of.len()]
    }

    /// Up to 40 characters of letters, digits, `+`, `@`, `:` and spaces: what a hostile server could
    /// put in a free-form attribute, phone numbers and JIDs included.
    fn junk(&mut self) -> String {
        const ALPHABET: &[u8] = b"abcXYZ0123456789+@:._- \xc3";
        let n = (self.next() % 40) as usize;
        (0..n)
            .map(|_| {
                let b = ALPHABET[(self.next() as usize) % ALPHABET.len()];
                if b == 0xc3 { 'é' } else { b as char }
            })
            .collect()
    }
}

fn reason_of(conn: &ConnState) -> Option<&str> {
    match conn {
        ConnState::Restricted { reason, .. } | ConnState::LoggedOut { reason, .. } => Some(reason),
        _ => None,
    }
}

fn has_digit_run(text: &str, n: usize) -> bool {
    text.as_bytes()
        .split(|b| !b.is_ascii_digit())
        .any(|run| run.len() >= n)
}

#[test]
fn no_reason_holds_anything_the_server_chose() {
    let mut rng = Lcg(0x5eed);
    let reasons = [
        ConnectFailureReason::LoggedOut,
        ConnectFailureReason::AccountLocked,
        ConnectFailureReason::UnknownLogout,
        ConnectFailureReason::Generic,
    ];
    let mut seen = 0usize;
    for _ in 0..1000 {
        // A hostile stanza: any tag we know, junk in every attribute a signal reads.
        let tag = rng.pick(&["failure", "stream:error", "iq"]);
        let junk = rng.junk();
        let mut b = NodeBuilder::new_dynamic(tag.to_owned())
            .attr("code", rng.pick(&["401", "516", "429", "999"]))
            .attr("violation_reason", junk.clone())
            .attr("message", rng.junk())
            .attr("location", rng.junk());
        if rng.next().is_multiple_of(3) {
            b = b.children([NodeBuilder::new("conflict")
                .attr("type", rng.pick(&["device_removed", "replaced", "weird"]))
                .build()]);
        }
        let reason = reasons[(rng.next() as usize) % reasons.len()];
        let logged_out = Event::LoggedOut(Box::new(
            LoggedOut::builder()
                .on_connect(tag == "failure")
                .reason(reason)
                .raw(b.build())
                .build(),
        ));
        let ban = Event::TemporaryBan(Box::new(
            TemporaryBan::builder()
                .code(TempBanReason::from((rng.next() % 200) as i32))
                .expire(ChronoDuration::try_seconds((rng.next() % 100_000) as i64).unwrap())
                .message(rng.junk())
                .url(rng.junk())
                .build(),
        ));
        for event in [logged_out, ban] {
            let Some(signal) = translate(&event, 1_700_000_000_000) else {
                continue;
            };
            let Some(s) = state_of(&signal) else { continue };
            let reason = reason_of(&s.conn).expect("a ban or a logout has a reason");
            seen += 1;
            assert!(
                !has_digit_run(reason, 6) && !reason.contains(['+', '@']),
                "reason {reason:?} from junk {junk:?}"
            );
            // The network's page, when the contract carries one, is an `https` address.
            if let ConnState::Restricted { note, .. } | ConnState::LoggedOut { note, .. } = &s.conn
            {
                assert!(
                    note.appeal_url
                        .as_deref()
                        .is_none_or(|url| url.starts_with("https://")),
                    "page {:?} from junk {junk:?}",
                    note.appeal_url
                );
            }
            // The violation reason, when kept, is a token.
            if let WaSignal::LoggedOut(l) = &signal
                && let Some(v) = &l.violation
            {
                assert!(
                    v.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                        && !has_digit_run(v, 6),
                    "violation {v:?}"
                );
            }
        }
    }
    assert!(seen > 1000, "the generator reached the reasons ({seen})");
}
