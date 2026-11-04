//! The online status's debounce and the network hints' rule (TG-2 C8c; critic H6, M8; lead T-3).
//! Pure: times are milliseconds on a made-up clock.

use kourier_backend_api::UiPresence;
use std::sync::Arc;

use kourier_model::ConvKey;

use super::*;

/// A ready presence that wants `want` at `now` (nothing told yet).
fn ready(want: bool, now: u64) -> Presence {
    let mut p = Presence::default();
    p.set_ready(true);
    p.on_ui(want, now);
    p
}

#[test]
fn online_needs_focus_and_this_accounts_open_chat_and_no_idle() {
    let open = Some(ConvKey::Group(Arc::from("tg:-1")));
    let ui = |focused, visible: Option<ConvKey>, idle| UiPresence {
        focused,
        visible,
        idle,
    };
    assert!(online_of(&ui(true, open.clone(), false)));
    // Another account's chat is open (the core fills `visible` only for the open account, T-3).
    assert!(!online_of(&ui(true, None, false)));
    assert!(!online_of(&ui(false, open.clone(), false)));
    assert!(!online_of(&ui(true, open, true)));
    assert!(!online_of(&UiPresence::default()));
}

#[test]
fn the_requests_are_telegrams_shapes() {
    assert_eq!(
        online_request(true),
        json!({"@type": "setOption", "name": "online",
               "value": {"@type": "optionValueBoolean", "value": true}})
    );
    assert_eq!(online_request(false)["value"]["value"], json!(false));
    assert_eq!(
        network_type_request(),
        json!({"@type": "setNetworkType", "type": {"@type": "networkTypeOther"}})
    );
}

#[test]
fn nothing_is_told_before_ready_and_online_comes_at_once_after_it() {
    let mut p = Presence::default();
    p.on_ui(true, 1_000);
    assert_eq!(p.poll(1_000), None, "never before Ready");
    assert_eq!(p.due_ms(), None);
    p.set_ready(true);
    assert_eq!(p.poll(1_000), Some(true), "at once at Ready");
    assert_eq!(p.poll(1_001), None, "told once");
    assert_eq!(p.due_ms(), None);
}

#[test]
fn an_unfocused_account_tells_nothing_at_ready() {
    // TDLib's default is offline: `false` is never sent first.
    let mut p = ready(false, 0);
    assert_eq!(p.poll(60_000), None);
    assert_eq!(p.due_ms(), None);
}

#[test]
fn offline_comes_five_seconds_after_the_focus_went() {
    let mut p = ready(true, 0);
    assert_eq!(p.poll(0), Some(true));
    p.on_ui(false, 10_000);
    assert_eq!(p.poll(10_000), None);
    assert_eq!(p.due_ms(), Some(10_000 + OFF_AFTER_MS));
    assert_eq!(p.poll(14_999), None);
    assert_eq!(p.poll(15_000), Some(false));
    assert_eq!(p.due_ms(), None);
}

#[test]
fn focus_back_within_five_seconds_tells_nothing() {
    let mut p = ready(true, 0);
    assert_eq!(p.poll(0), Some(true));
    p.on_ui(false, 10_000);
    p.on_ui(true, 12_000);
    assert_eq!(p.poll(12_000), None);
    assert_eq!(p.poll(20_000), None);
    assert_eq!(p.due_ms(), None);
}

#[test]
fn focus_flapping_sets_online_at_most_once_per_two_seconds() {
    // The critic's H6 test: alt-tab every 600 ms for a minute.
    let mut p = ready(false, 0);
    let mut told: Vec<(u64, bool)> = Vec::new();
    let mut now = 0;
    let mut focused = false;
    while now <= 60_000 {
        if now % 600 == 0 {
            focused = !focused;
            p.on_ui(focused, now);
        }
        if let Some(v) = p.poll(now) {
            told.push((now, v));
        }
        now += 100;
    }
    for pair in told.windows(2) {
        assert!(
            pair[1].0 - pair[0].0 >= MIN_GAP_MS,
            "two changes within {} ms",
            pair[1].0 - pair[0].0
        );
    }
    // Never more than 600 ms without focus, so never offline: online once, then nothing.
    assert_eq!(told, vec![(0, true)]);
}

#[test]
fn a_quick_return_to_online_waits_for_the_gap() {
    let mut p = ready(true, 0);
    assert_eq!(p.poll(0), Some(true));
    p.on_ui(false, 0);
    assert_eq!(p.poll(OFF_AFTER_MS), Some(false));
    p.on_ui(true, OFF_AFTER_MS + 500);
    assert_eq!(p.poll(OFF_AFTER_MS + 500), None, "the gap");
    assert_eq!(p.due_ms(), Some(OFF_AFTER_MS + MIN_GAP_MS));
    assert_eq!(p.poll(OFF_AFTER_MS + MIN_GAP_MS), Some(true));
}

#[test]
fn a_stop_tells_offline_only_when_online_was_told_and_the_next_helper_starts_over() {
    let mut p = ready(true, 0);
    assert!(
        !p.would_take_back(),
        "nothing told yet: nothing to take back"
    );
    assert_eq!(p.poll(0), Some(true));
    assert!(p.on_stop(3_000), "online was told: offline before the stop");
    assert!(!p.on_stop(3_001), "once");
    // The next helper's TDLib starts offline: online again once it is Ready, after the gap.
    assert_eq!(p.poll(4_000), None, "not ready yet");
    p.set_ready(true);
    assert_eq!(p.poll(4_000), None, "the stop's offline counts for the gap");
    assert_eq!(p.poll(5_000), Some(true));
}

#[test]
fn a_network_change_is_passed_on_only_while_not_ready_and_a_resume_always() {
    let mut h = Hints::default();
    assert!(!h.on_hint(Hint::NetworkChanged, Some(ConnectionState::Ready), 0));
    assert!(h.on_hint(Hint::NetworkChanged, Some(ConnectionState::Connecting), 0));
    let mut h = Hints::default();
    assert!(h.on_hint(Hint::NetworkChanged, None, 0));
    let mut h = Hints::default();
    assert!(h.on_hint(Hint::Resumed, Some(ConnectionState::Ready), 0));
}

#[test]
fn network_types_go_at_most_once_per_ten_seconds() {
    let mut h = Hints::default();
    assert!(h.on_hint(Hint::Resumed, None, 1_000));
    assert!(!h.on_hint(Hint::Resumed, None, 1_000 + HINT_GAP_MS - 1));
    assert!(!h.on_hint(
        Hint::NetworkChanged,
        Some(ConnectionState::WaitingForNetwork),
        5_000
    ));
    assert!(h.on_hint(Hint::Resumed, None, 1_000 + HINT_GAP_MS));
    // A hint that was not passed on does not move the window.
    let next = 1_000 + 2 * HINT_GAP_MS;
    assert!(!h.on_hint(Hint::NetworkChanged, Some(ConnectionState::Ready), next));
    assert!(h.on_hint(Hint::Resumed, None, next));
}

impl Presence {
    /// What a stop would answer now, without stopping.
    fn would_take_back(&self) -> bool {
        self.told == Some(true)
    }
}
