//! THE JUDGE (T5): one table test per stop rule (S1-S12 of `briefs/tr.md` §2.10 with the critic's
//! changes), fed the stand-in app's lines (tracing's plain format, as the app prints them) and the
//! dev events in the reader's words; then the halt's END through the supervisor and the stand-in app
//! (the app asked to quit, the switch off after its exit; a link let finish; a waiting login closed;
//! the drop's outcome waited for, critic M2). No wall time is asserted: the ends wait on outcomes.

use std::time::Duration;

use serde_json::{Value, json};

use super::lines::{at, count_line};
use super::supervise::{Rig, quick};
use crate::threema_run::judge::{
    COUNT_LINES_WITHIN, Judge, SEND_BOXES, SEND_REFLECTS, Said, Seen, count_lines_missing, judge,
};
use crate::threema_run::launch::QUESTION_KIND;
use crate::threema_run::lines::parse;
use crate::threema_run::prompt::{drop_preface, question_kind};

const T: &str = "kourier_threema::conn";
const MINUTE: u64 = 60_000;

/// A run as the judge sees it: its context and the time.
struct W {
    j: Judge,
    t: u64,
}

impl W {
    fn new() -> Self {
        Self {
            j: Judge::default(),
            t: 1_000_000,
        }
    }

    fn removal() -> Self {
        let mut w = Self::new();
        w.j.removal_asked = true;
        w
    }

    fn see(&mut self, seen: &Seen<'_>) -> Option<String> {
        judge(&mut self.j, seen, self.t)
    }

    fn line(&mut self, line: &str) -> Option<String> {
        let parsed = parse(line).unwrap_or_else(|| panic!("a line the tool reads: {line}"));
        self.see(&Seen::Line(&parsed))
    }

    fn warn(&mut self, text: &str) -> Option<String> {
        self.line(&at("WARN", T, text))
    }

    fn info(&mut self, text: &str) -> Option<String> {
        self.line(&at("INFO", T, text))
    }

    fn count(&mut self, when: &str, set: &[(&str, u64)]) -> Option<String> {
        self.line(&count_line(when, set, &[]))
    }

    fn held_count(
        &mut self,
        when: &str,
        set: &[(&str, u64)],
        held_by: &[(&str, u64)],
    ) -> Option<String> {
        self.line(&count_line(when, set, held_by))
    }

    fn step(&mut self, step: &str, kind: &str) -> Option<String> {
        self.see(&Seen::LoginStep { step, kind })
    }

    fn error(&mut self, kind: &str) -> Option<String> {
        self.see(&Seen::LoginError { kind })
    }

    fn account(&mut self, state: &str, conn: &str) -> Option<String> {
        self.see(&Seen::Account { state, conn })
    }

    fn exit(&mut self, asked: bool) -> Option<String> {
        self.see(&Seen::AppExited { asked })
    }
}

fn some(kind: &str) -> Option<String> {
    Some(kind.to_owned())
}

/// S1a: a failure at `qr` or `emoji`, or the refusal of the ID at `linking`; at `qr` the refusal is
/// B1's (`list-not-taken`). The error event that follows the failed step decides; a line between
/// does not; the next event decides without it.
#[test]
fn s1a_a_login_refused_before_anything_was_registered() {
    let rows: [(&str, &str, Option<String>); 5] = [
        ("qr", "NotTheExpectedAccount", some("list-not-taken")),
        ("qr", "Transient", some("link-refused")),
        ("emoji", "Unauthorized", some("link-refused")),
        ("emoji", "Expired", some("link-refused")),
        ("linking", "NotTheExpectedAccount", some("link-refused")),
    ];
    for (step, kind, want) in rows {
        let mut w = W::new();
        assert_eq!(w.step(step, step), None);
        assert_eq!(w.step(step, "failed"), None, "{step}: the error decides");
        assert_eq!(w.count("running", &[]), None, "{step}: a line between");
        assert_eq!(w.error(kind), want, "{step} {kind}");
        assert_eq!(w.j.drop_owed_since_ms, None, "{step}: nothing to drop");
    }
    // The failed step alone: the next event decides it without the kind.
    let mut w = W::new();
    w.step("qr", "failed");
    assert_eq!(w.account("linking", "offline"), some("link-refused"));
}

/// S1b: any other failure at `linking` (the core's logout drops the device: the end waits for it),
/// and the app gone at `linking`: a crash, a kill, a stop (RP-b's review S-1).
#[test]
fn s1b_a_link_that_failed_after_its_hello() {
    for kind in ["Internal", "Unsupported", "Transient", ""] {
        let mut w = W::new();
        w.step("linking", "progress");
        w.step("linking", "failed");
        assert_eq!(w.error(kind), some("link-failed-after-hello"), "{kind}");
        assert_eq!(
            w.j.drop_owed_since_ms,
            Some(w.t),
            "{kind}: the drop is owed"
        );
    }
    for asked in [false, true] {
        let mut w = W::new();
        w.step("emoji", "emoji");
        w.step("linking", "progress");
        assert_eq!(w.exit(asked), some("link-failed-after-hello"), "{asked}");
        assert_eq!(w.j.drop_owed_since_ms, None, "a dead app drops nothing");
    }
    // A failed step at linking whose error never came: the app's exit decides it.
    let mut w = W::new();
    w.step("linking", "failed");
    assert_eq!(w.exit(true), some("link-failed-after-hello"));
    // A link that ended `done` is no question.
    let mut w = W::new();
    w.step("linking", "progress");
    w.step("done", "done");
    assert_eq!(w.exit(true), None);
}

/// S2: he says the emoji differ and cancels on the phone: the login fails at `emoji` (S1a's row).
#[test]
fn s2_emoji_that_differ_end_at_emoji() {
    let mut w = W::new();
    w.step("emoji", "emoji");
    w.step("emoji", "failed");
    assert_eq!(w.error("Unauthorized"), some("link-refused"));
}

/// S3: the account needs action, is restricted or logged out; a removal he asked for is its own.
#[test]
fn s3_the_account_needs_action_is_restricted_or_logged_out() {
    let rows = [
        ("active", "needs_action", some("needs-action")),
        ("active", "restricted", some("restricted")),
        ("logged_out", "offline", some("logged-out")),
        ("active", "logged_out", some("logged-out")),
        ("active", "online", None),
        ("active", "syncing", None),
        ("active", "connecting", None),
        ("unlinking", "offline", None),
    ];
    for (state, conn, want) in rows {
        assert_eq!(W::new().account(state, conn), want, "{state} {conn}");
    }
    assert_eq!(W::removal().account("logged_out", "logged_out"), None);
    assert_eq!(
        W::removal().account("active", "needs_action"),
        some("needs-action")
    );
}

fn closed(code: u16) -> String {
    format!("the mediator closed the connection code={code} meaning=\"some words\"")
}

/// S4: the closes 4110-4115 and the codes the spec does not name, a CSP close-error, E3, E7, E8;
/// 4113 inside an asked removal and 4113/4114 after the drop went on the live socket are not halts.
#[test]
fn s4_a_close_the_run_cannot_go_on_after() {
    for code in 4110..=4115 {
        assert_eq!(
            W::new().info(&closed(code)),
            Some(format!("close-{code}")),
            "{code}"
        );
    }
    for code in [4999, 4100, 1002] {
        assert_eq!(W::new().info(&closed(code)), Some(format!("close-{code}")));
    }
    for code in [1000, 1001, 1011, 4000, 4001, 4009, 4010, 4011, 4012, 4013] {
        assert_eq!(W::new().info(&closed(code)), None, "{code}");
    }
    assert_eq!(
        W::removal().info(&closed(4113)),
        None,
        "removed on the phone"
    );
    assert_eq!(W::removal().info(&closed(4114)), some("close-4114"));
    for code in [4113, 4114] {
        let mut w = W::new();
        assert_eq!(
            w.info("the own device's drop went on the live socket"),
            None
        );
        assert_eq!(w.info(&closed(code)), None, "the drop's own end");
        assert_eq!(w.j.drop, Some("removed"));
    }
    assert_eq!(
        W::new().count("end", &[("in_close_error", 1)]),
        some("close-error")
    );
    let rows = [
        (
            "the server's certificate is not one Kourier trusts kind=PinMismatch",
            "untrusted",
        ),
        (
            "the mediator did not register the joined device as new",
            "not-registered",
        ),
        (
            "the core refused the joined account's first row; the link ends",
            "first-row-refused",
        ),
    ];
    for (text, want) in rows {
        assert_eq!(W::new().warn(text), some(want), "{text}");
    }
}

/// S5a, a FOLLOWER socket: only reflected-acks, echoes, the idle timeout and a job's frames (R-H2:
/// a job left from a leader socket may go here); a chat server message read, the H1 guard, any
/// other write halt; a transmit frame halts on any socket.
#[test]
fn s5a_a_follower_writes_nothing_but_its_acks_and_a_jobs_frames() {
    let fine: [&[(&str, u64)]; 4] = [
        &[],
        &[("out_reflected_ack", 9), ("out_echo", 4), ("out_idle", 1)],
        &[
            ("in_reflected", 9),
            ("in_reflect_ack", 2),
            ("in_dry", 1),
            ("in_echo", 4),
        ],
        // A receipt job left from the leader socket: two reflects and its box.
        &[
            ("out_reflect_job", 2),
            ("out_box_job", 1),
            ("in_reflect_ack", 2),
        ],
    ];
    for set in fine {
        for when in ["running", "end"] {
            assert_eq!(W::new().count(when, set), None, "{when} {set:?}");
        }
    }
    let rows: [(&[(&str, u64)], &str); 11] = [
        (&[("in_csp_message", 1)], "unasked"),
        (&[("out_box_transmit", 1)], "transmit"),
        (&[("out_reflect_transmit", 1)], "transmit"),
        (&[("out_message_ack", 1)], "follower-wrote"),
        (&[("out_reflect_inbound", 1)], "follower-wrote"),
        (&[("out_ephemeral", 1)], "follower-wrote"),
        (&[("out_tx", 1)], "follower-wrote"),
        (&[("out_other", 1)], "follower-wrote"),
        (&[("out_drop", 1)], "follower-wrote"),
        // A job's reflects beyond two per box and the one row in flight.
        (
            &[("out_reflect_job", 5), ("out_box_job", 1)],
            "follower-wrote",
        ),
        (&[("out_reflect_job", 3)], "follower-wrote"),
    ];
    for (set, want) in rows {
        assert_eq!(W::new().count("end", set), some(want), "{set:?}");
    }
    assert_eq!(
        W::new().warn("a chat server message came unasked; closing"),
        some("unasked")
    );
    assert_eq!(
        W::removal().count("end", &[("out_drop", 1)]),
        None,
        "an asked unlink"
    );
}

/// S5b, a LEADER socket (`out_unblock` 1 or D1 on it): within the incoming count; a second unblock or
/// D1, acks or inbound reflects beyond the messages, a burst of boxes or a job's reflects, `other`,
/// an unasked drop halt.
#[test]
fn s5b_a_leader_writes_within_what_came_in() {
    // The row's own counters first: the line takes the first value of a name.
    let lead = |set: &[(&'static str, u64)]| {
        let mut all = set.to_vec();
        all.push(("out_unblock", 1));
        all
    };
    let within = lead(&[
        ("in_csp_message", 2),
        ("out_message_ack", 2),
        ("out_reflect_inbound", 2),
        ("out_ephemeral", 1),
        ("out_box_job", 2),
        ("out_reflect_job", 4),
        ("out_reflected_ack", 5),
        ("out_echo", 3),
        ("out_idle", 1),
    ]);
    assert_eq!(W::new().count("end", &within), None);
    let rows: [&[(&str, u64)]; 8] = [
        &[("out_unblock", 2)],
        &[("in_csp_message", 2), ("out_message_ack", 3)],
        &[("in_csp_message", 2), ("out_reflect_inbound", 3)],
        &[("in_csp_message", 0), ("out_ephemeral", 1)],
        &[("in_csp_message", 2), ("out_box_job", 10)],
        &[
            ("in_csp_message", 2),
            ("out_box_job", 1),
            ("out_reflect_job", 5),
        ],
        &[("out_other", 1)],
        &[("out_drop", 1)],
    ];
    for set in rows {
        assert_eq!(
            W::new().count("end", &lead(set)),
            some("leader-burst"),
            "{set:?}"
        );
    }
    assert_eq!(
        W::new().count("end", &lead(&[("out_box_transmit", 1)])),
        some("transmit")
    );
    // D1 makes the socket a leader before its count line says so; a second D1 is a burst; the
    // socket's end takes the role away.
    let mut w = W::new();
    assert_eq!(
        w.info("incoming messages were unblocked as the leader"),
        None
    );
    assert!(w.j.leader);
    assert_eq!(
        w.count(
            "running",
            &[
                ("in_csp_message", 1),
                ("out_message_ack", 1),
                ("out_reflect_inbound", 1)
            ]
        ),
        None,
        "a leader by its D1"
    );
    assert_eq!(
        w.info("incoming messages were unblocked as the leader"),
        some("leader-burst")
    );
    let mut w = W::new();
    w.info("incoming messages were unblocked as the leader");
    assert_eq!(w.count("end", &[]), None);
    assert!(!w.j.leader, "the next socket starts as a follower");
    assert_eq!(
        w.info("incoming messages were unblocked as the leader"),
        None,
        "one D1 per socket"
    );
    assert_eq!(
        W::removal().count("end", &lead(&[("out_drop", 1)])),
        None,
        "an asked unlink"
    );
}

/// S5c: a payload Kourier does not handle (H1's kind line, or `in_other` without it).
#[test]
fn s5c_an_unhandled_kind_halts() {
    assert_eq!(
        W::new().info("a server payload Kourier does not handle server=chat kind=ServerAlert"),
        some("unhandled-ServerAlert")
    );
    assert_eq!(
        W::new().info("a server payload Kourier does not handle server=mediator kind=DevicesInfo"),
        some("unhandled-DevicesInfo")
    );
    assert_eq!(
        W::new().count("end", &[("in_other", 1)]),
        some("unhandled-other")
    );
}

/// S5d: a held group change STOPS the run (his TH6-Q1 answer: it stops safely), by its D2 line or by
/// `held` in a count line (the rest of the hold words: `s5d_each_hold_word_has_its_verdict`).
#[test]
fn s5d_a_held_message_stops_the_run() {
    assert_eq!(
        W::new().info("a chat server message is held for the phone rule=group-sync-needed"),
        some("held-group-sync-needed")
    );
    let line = count_line(
        "running",
        &[("out_unblock", 1), ("in_csp_message", 1)],
        &[("group-sync-needed", 1)],
    );
    assert_eq!(W::new().line(&line), some("held-group-sync-needed"));
}

const HELD: &str = "a chat server message is held for the phone rule=";
const DEVICES_KIND: &str =
    "a server payload Kourier does not handle server=mediator kind=DevicesInfo";
const ASIDE: &str = "messages are held for the phone and another device is online; stepping aside so it can lead held=1";

/// S5d after TH6 FIX-A: every hold word of `th6-lines.md` ("After L6", "After the fix round (TH6
/// FIX-A)": twelve) has its verdict by the D2 line and by the count line: a group change (and Kourier's
/// own failures, the two `inbound-reflected-*` included, and what resolves itself on a later socket)
/// STOPS, the two holds only the phone resolves and `inbound-known` stand; a word the tool does not
/// know stops.
#[test]
fn s5d_each_hold_word_has_its_verdict() {
    use crate::threema_run::judge::{HOLD_RULES, TOLERATED_HOLDS};
    for rule in HOLD_RULES {
        let tolerated = TOLERATED_HOLDS.contains(&rule);
        let want = (!tolerated).then(|| format!("held-{rule}"));
        assert_eq!(
            W::new().info(&format!("{HELD}{rule}")),
            want,
            "the D2 line of {rule}"
        );
        assert_eq!(
            W::new().held_count("running", &[("out_unblock", 1)], &[(rule, 1)]),
            want,
            "the count line of {rule}"
        );
    }
    assert_eq!(HOLD_RULES.len(), 12);
    assert_eq!(
        TOLERATED_HOLDS,
        [
            "csp-unknown-type",
            "creator-answer-needs-picture",
            "inbound-known"
        ],
        "the two the step-aside hands to the phone, and a second copy of a known message"
    );
    // The store failure and the full job budget of a reflected row STOP (a run of RT1 reaches neither).
    for rule in [
        "inbound-reflected-zone-failed",
        "inbound-reflected-queue-full",
    ] {
        assert!(!TOLERATED_HOLDS.contains(&rule));
        assert_eq!(
            W::new().info(&format!("{HELD}{rule}")),
            some(&format!("held-{rule}"))
        );
    }
    // A rule the tool does not know, and the two words L5 and L6 took away, stop.
    for rule in ["csp-sender-lookup", "inbound-jobs-pending", "something-new"] {
        assert!(!HOLD_RULES.contains(&rule));
        assert_eq!(
            W::new().info(&format!("{HELD}{rule}")),
            some(&format!("held-{rule}"))
        );
    }
    // A count whose rules are a tolerated one and a stopping one stops on the stopping one, in
    // either order of the line.
    for by in [
        [("csp-unknown-type", 1), ("group-sync-needed", 1)],
        [("group-sync-needed", 1), ("csp-unknown-type", 1)],
    ] {
        assert_eq!(
            W::new().held_count("running", &[("out_unblock", 1)], &by),
            some("held-group-sync-needed")
        );
    }
}

/// T-H1's step-aside, case by case: the ask (`out_other`), the answer (`in_other` and the kind line),
/// the INFO line and the socket's end are no halt while a tolerated hold stands; the same frames with
/// no hold, on a follower, in excess of the asks, or with a group change still stop.
#[test]
fn the_step_aside_is_no_halt_and_a_held_group_change_still_is() {
    // The whole story on one leader socket: hold, ask, answer, the line, the end.
    let mut w = W::new();
    assert_eq!(
        w.info("incoming messages were unblocked as the leader"),
        None
    );
    assert_eq!(w.info(&format!("{HELD}csp-unknown-type")), None);
    assert_eq!(
        w.info(DEVICES_KIND),
        None,
        "the answer to the step-aside's ask"
    );
    assert_eq!(w.info(ASIDE), None);
    assert_eq!(w.j.asides, 1);
    let end = [
        ("out_unblock", 1),
        ("in_csp_message", 1),
        ("out_message_ack", 1),
        ("out_reflect_inbound", 1),
        ("out_other", 1),
        ("in_other", 1),
    ];
    assert_eq!(
        w.held_count("end", &end, &[("csp-unknown-type", 1)]),
        None,
        "the step-aside's end line"
    );
    // Its end is no loop: ends of step-asides never fill S6's windows.
    for _ in 0..8 {
        w.t += MINUTE;
        w.info(&format!("{HELD}csp-unknown-type"));
        w.info(ASIDE);
        assert_eq!(w.held_count("end", &end, &[("csp-unknown-type", 1)]), None);
    }
    // The next socket starts clean: a DevicesInfo kind with no hold is unasked again.
    assert_eq!(w.info(DEVICES_KIND), some("unhandled-DevicesInfo"));

    // The kind line alone, the counts alone, on a follower: stop as before.
    assert_eq!(
        W::new().held_count("end", &[("out_unblock", 1), ("out_other", 1)], &[]),
        some("leader-burst")
    );
    assert_eq!(
        W::new().held_count("end", &[("in_other", 1), ("out_other", 1)], &[]),
        some("unhandled-other")
    );
    let follower = [("out_other", 1), ("in_other", 1)];
    let mut f = W::new();
    f.info(&format!("{HELD}csp-unknown-type"));
    assert_eq!(
        f.held_count("running", &follower, &[("csp-unknown-type", 1)]),
        some("follower-wrote"),
        "a follower never asks for the devices"
    );
    // Another kind is still a stop, with a step-aside under way.
    let mut k = W::new();
    k.info(&format!("{HELD}csp-unknown-type"));
    assert_eq!(
        k.info("a server payload Kourier does not handle server=chat kind=ServerAlert"),
        some("unhandled-ServerAlert")
    );
    assert_eq!(
        k.info("a server payload Kourier does not handle server=mediator kind=UnknownPayload"),
        some("unhandled-UnknownPayload")
    );
    // The asks are bounded: one a minute since the hold and two, every answer after an ask.
    let mut b = W::new();
    b.info(&format!("{HELD}csp-unknown-type"));
    let hold = &[("csp-unknown-type", 1)];
    let unblocked = |other: u64, answers: u64| {
        vec![
            ("out_unblock", 1),
            ("out_other", other),
            ("in_other", answers),
        ]
    };
    assert_eq!(b.held_count("running", &unblocked(2, 2), hold), None);
    assert_eq!(
        b.held_count("running", &unblocked(3, 0), hold),
        some("leader-burst"),
        "three asks in the first minute"
    );
    assert_eq!(
        b.held_count("running", &unblocked(3, 3), hold),
        some("unhandled-other"),
        "and their answers"
    );
    b.t += 10 * MINUTE;
    assert_eq!(b.held_count("running", &unblocked(12, 12), hold), None);
    assert_eq!(
        b.held_count("running", &unblocked(3, 4), hold),
        some("unhandled-other"),
        "an answer with no ask"
    );
    // A held group change still stops the run, before or after the step-aside's frames.
    let mut g = W::new();
    g.info(&format!("{HELD}csp-unknown-type"));
    assert_eq!(
        g.info(&format!("{HELD}group-sync-needed")),
        some("held-group-sync-needed")
    );
    let mut g = W::new();
    assert_eq!(g.info(ASIDE), None);
    assert_eq!(
        g.held_count(
            "end",
            &end,
            &[("csp-unknown-type", 1), ("group-held-before", 1)]
        ),
        some("held-group-held-before")
    );
}

/// `inbound-known` is a hold that stands (a second copy of a known message, harmless, resumed by the
/// next socket) but it is not one the step-aside hands to the phone: it opens no allowance for the
/// leader's `GetDevicesInfo`, its answer or its `out_other`, alone or beside a stopping word.
#[test]
fn inbound_known_stands_and_opens_no_step_aside_allowance() {
    let known = &[("inbound-known", 1)];
    let ask = [("out_unblock", 1), ("out_other", 1), ("in_other", 1)];
    // The hold alone: no halt, by the D2 line and by the count line, a leader or not.
    let mut w = W::new();
    assert_eq!(w.info(&format!("{HELD}inbound-known")), None);
    assert_eq!(w.held_count("running", &[("out_unblock", 1)], known), None);
    assert_eq!(w.j.asides, 0);
    // But no ask is the step-aside's: the mediator's answer, the counts of an ask.
    assert_eq!(w.info(DEVICES_KIND), some("unhandled-DevicesInfo"));
    let mut w = W::new();
    assert_eq!(
        w.held_count("running", &[("out_unblock", 1), ("out_other", 1)], known),
        some("leader-burst"),
        "an ask beside `inbound-known` alone"
    );
    let mut w = W::new();
    w.info(&format!("{HELD}inbound-known"));
    assert_eq!(
        w.held_count("running", &[("in_other", 1), ("out_other", 1)], known),
        some("unhandled-other"),
        "an answer beside `inbound-known` alone"
    );
    // Beside a hold the phone resolves the allowance is that hold's, as before.
    let both = &[("csp-unknown-type", 1), ("inbound-known", 1)];
    assert_eq!(W::new().held_count("running", &ask, both), None);
    let mut w = W::new();
    w.info(&format!("{HELD}csp-unknown-type"));
    w.info(&format!("{HELD}inbound-known"));
    assert_eq!(w.info(DEVICES_KIND), None);
    // Beside a stopping word the stopping word is the halt.
    assert_eq!(
        W::new().held_count(
            "running",
            &[("out_unblock", 1)],
            &[("inbound-known", 1), ("inbound-reflected-queue-full", 1)]
        ),
        some("held-inbound-reflected-queue-full")
    );
}

/// S6: more than 3 socket ends in 10 minutes or 13 in an hour that no stop caused.
#[test]
fn s6_a_loop_of_sockets_halts() {
    let mut w = W::new();
    for _ in 0..3 {
        assert_eq!(w.count("end", &[]), None);
        w.t += 2 * MINUTE;
    }
    assert_eq!(
        w.count("end", &[]),
        some("socket-loop"),
        "the fourth in 10 minutes"
    );
    // Every 4.5 minutes: never 4 in 10, the 14th in an hour.
    let mut w = W::new();
    for _ in 0..13 {
        assert_eq!(w.count("end", &[]), None);
        w.t += 4 * MINUTE + MINUTE / 2;
    }
    assert_eq!(
        w.count("end", &[]),
        some("socket-loop"),
        "the 14th in an hour"
    );
    // Every 5 minutes for ever: the old ends leave the hour.
    let mut w = W::new();
    for _ in 0..40 {
        assert_eq!(w.count("end", &[]), None);
        w.t += 5 * MINUTE;
    }
    // A running line is no end; ends while the run is ending are the stop's.
    let mut w = W::new();
    for _ in 0..10 {
        assert_eq!(w.count("running", &[]), None);
    }
    w.j.ending = true;
    for _ in 0..10 {
        assert_eq!(w.count("end", &[]), None);
    }
}

/// S7, S9, S12: a step's verdict or his answer.
#[test]
fn s7_s9_s12_what_a_step_or_his_answer_says() {
    let rows = [
        (Said::Lost, "lost"),
        (Said::Twice, "twice"),
        (Said::PhoneWarning, "phone-warning"),
        (Said::Stop, "stopped"),
    ];
    for (said, want) in rows {
        assert_eq!(W::new().see(&Seen::Said(said)), some(want));
    }
}

/// S8: an ERROR line of the backend or the core, E5, a panic, the app's exit nobody asked for.
#[test]
fn s8_an_error_a_panic_or_an_exit_nobody_asked_for() {
    let rows = [
        (
            at("ERROR", "kourier_threema::actor", "the actor failed"),
            some("error"),
        ),
        (
            at("ERROR", "kourier_core::ingest", "a write failed"),
            some("error"),
        ),
        (
            "2026-11-04T10:00:00Z ERROR actor{account=1}: kourier_core: broke".to_owned(),
            some("error"),
        ),
        (
            at("ERROR", T, "the connection task panicked"),
            some("error"),
        ),
        (
            "thread 'tokio-runtime-worker' panicked at crates/x/src/y.rs:1:1".to_owned(),
            some("error"),
        ),
        (at("ERROR", "kourier_qt", "a QML error"), None),
        (at("ERROR", "kourier_app::desktop", "no tray"), None),
    ];
    for (line, want) in rows {
        assert_eq!(W::new().line(&line), want, "{line}");
    }
    assert_eq!(W::new().exit(false), some("exited"));
    assert_eq!(W::new().exit(true), None);
}

/// S10: E6, the list of test IDs refused by the app. S11: E2, the clock.
#[test]
fn s10_s11_the_list_refused_and_the_clock() {
    let e6 = at(
        "WARN",
        "kourier_app::threema",
        "the list of Threema test IDs is not used problem=the file is not 0600",
    );
    assert_eq!(W::new().line(&e6), some("list-not-taken"));
    assert_eq!(
        W::new().warn("the clock is off by more than 20 minutes server=\"mediator\" minutes=31"),
        some("clock")
    );
}

/// A line the rules read whose fields are not the contract's: a blind judge is no run. And what is
/// not a halt: E4, D1's first line, D4's lines (which keep the drop's outcome).
#[test]
fn an_unreadable_line_halts_and_the_lines_that_are_no_halt() {
    let whole = count_line("end", &[], &[]);
    assert_eq!(
        W::new().line(&whole.replace(" in_dry=0", "")),
        some("line-unreadable")
    );
    assert_eq!(
        W::new().info("the mediator closed the connection meaning=\"no code\""),
        some("line-unreadable")
    );
    let mut w = W::new();
    let quiet = [
        "the server's message was refused what=\"d2m\" unsupported=false",
        "incoming messages were unblocked as the leader",
        "the own device's drop went on the live socket",
        "the own device's drop could not be written to the live socket",
        "no socket to drop the own device on: closed",
        "the mediator did not answer the drop in time written=true",
    ];
    for text in quiet {
        assert_eq!(w.info(text), None, "{text}");
    }
    assert_eq!(
        w.j.drop, None,
        "no outcome yet: the D2M-only socket's line follows"
    );
    assert_eq!(
        w.info("the own device's drop on a socket of its own outcome=NotReached"),
        None
    );
    assert_eq!(w.j.drop, Some("not-reached"));
    let mut w = W::new();
    w.info("the own device's drop on a socket of its own outcome=Removed");
    assert_eq!(w.j.drop, Some("removed"));
    let mut w = W::new();
    w.info("the mediator acked the own device's drop");
    assert_eq!(w.j.drop, Some("removed"));
    let mut w = W::new();
    w.info("the mediator did not answer the drop in time");
    assert_eq!(w.j.drop, Some("not-reached"), "the live socket's: final");
}

// ---- The halt's end, through the supervisor and the stand-in app ------------------------------------

/// THE HALT'S END (the mutant's test, critic R-L5): a line that halts ends the run BY ITSELF: the
/// app is asked to quit and goes, and only then is the switch written off; the halt is kept.
#[test]
fn a_halt_ends_the_run_by_itself_the_app_quits_and_then_the_switch_goes_off() {
    let rig = Rig::new("j-halt");
    let run = rig.run("unasked", false, quick());
    let code = run
        .join_within(Duration::from_secs(20))
        .expect("the halt did not end the run: the app still runs")
        .expect("ended");
    assert_eq!(code, 1, "a halted run");
    assert_eq!(rig.state()["halted"]["kind"], json!("unasked"));
    assert_eq!(
        rig.seen_commands(),
        ["quit"],
        "the end asked the app to quit"
    );
    assert!(
        rig.app_saw_the_switch_on(),
        "the switch was still on at the app's last moment"
    );
    rig.assert_ended("a halt");
}

/// A halt while the login waits at its code: the login is closed (the page change to the list)
/// before the app is asked to quit.
#[test]
fn a_halt_while_the_code_waits_closes_the_login_first() {
    let rig = Rig::new("j-qr");
    let run = rig.run("qr-halt", false, quick());
    let code = run
        .join_within(Duration::from_secs(20))
        .expect("the halt ended the run")
        .expect("ended");
    assert_eq!(code, 1);
    assert_eq!(rig.seen_commands(), ["page", "quit"]);
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events");
    assert!(events.contains("\"login_closed\""), "{events}");
    rig.assert_ended("a halt at the code");
}

/// A halt while a link the phone confirmed stands at `linking`: the link is let finish, then the
/// app is asked to quit; the account is kept and no question is opened.
#[test]
fn a_halt_lets_a_link_at_linking_finish() {
    let rig = Rig::new("j-linking");
    let run = rig.run("linking-halt", false, quick());
    let code = run
        .join_within(Duration::from_secs(20))
        .expect("the halt ended the run")
        .expect("ended");
    assert_eq!(code, 1);
    let state = rig.state();
    assert_eq!(state["halted"]["kind"], json!("not-registered"));
    assert!(
        state["linked_since_ms"].is_u64(),
        "the link finished: {state}"
    );
    assert_eq!(state["link_at"], Value::Null);
    assert_eq!(question_kind(&rig.dir), None);
    assert_eq!(rig.seen_commands(), ["quit"]);
    rig.assert_ended("a halt at linking");
}

/// S1b with M2: a link that failed after its hello halts; the end waits for RP-b's drop outcome line
/// before `quit` (a quit first would cancel the drop), keeps it in the halt, and the question in the
/// keeper root says it before it asks. With no outcome line in time: "not confirmed".
#[test]
fn a_link_that_failed_after_its_hello_waits_for_the_drop_and_the_question_says_it() {
    let rig = Rig::new("j-s1b");
    let run = rig.run("s1b-drop", false, quick());
    assert_eq!(
        run.join_within(Duration::from_secs(20))
            .expect("the halt ended the run")
            .expect("ended"),
        1
    );
    let state = rig.state();
    assert_eq!(state["halted"]["kind"], json!("link-failed-after-hello"));
    assert_eq!(
        state["halted"]["drop"],
        json!("removed"),
        "the end waited for the drop's line: {state}"
    );
    assert!(state["link_at"].is_u64());
    assert_eq!(question_kind(&rig.dir).as_deref(), Some(QUESTION_KIND));
    assert!(drop_preface(&rig.dir).contains("Removed"));
    assert_eq!(rig.seen_commands(), ["quit"]);
    rig.assert_ended("a failed link");

    let rig = Rig::new("j-s1b-silent");
    let timing = crate::threema_run::supervise::Timing {
        drop_wait: Duration::from_millis(300),
        ..quick()
    };
    let run = rig.run("s1b-silent", false, timing);
    assert_eq!(
        run.join_within(Duration::from_secs(20))
            .expect("the halt ended the run")
            .expect("ended"),
        1
    );
    assert_eq!(rig.state()["halted"]["drop"], json!("not-confirmed"));
    assert_eq!(question_kind(&rig.dir).as_deref(), Some(QUESTION_KIND));
    assert!(drop_preface(&rig.dir).contains("not confirmed"));
    rig.assert_ended("a failed link with no drop line");
}

// ---- The security review's two MEDIUMs and LOW-9 ---------------------------------------------------------

/// MEDIUM-2: a FOLLOWER socket's job boxes have a RUN-WIDE budget: three per message the leader
/// sockets took, and three more. Every box is a chat server message to a real contact; a job's reflects
/// are bounded by its boxes, so more boxes would only have loosened that bound. The ended sockets'
/// boxes are in the total, this socket's own count from its open (a running line is a partial read).
/// Mutant: the budget off (`boxes_over_budget` false) kills this test.
#[test]
fn s5a_a_followers_job_boxes_have_a_run_wide_budget() {
    // RT1's first follower sockets (no leader yet): three boxes at most, the reviewer's numbers halt.
    for when in ["running", "end"] {
        assert_eq!(
            W::new().count(when, &[("out_box_job", 4), ("out_reflect_job", 8)]),
            some("follower-wrote"),
            "{when}"
        );
        assert_eq!(
            W::new().count(when, &[("out_box_job", 3), ("out_reflect_job", 6)]),
            None,
            "{when}"
        );
    }
    // A leader socket took two messages and wrote two boxes: nine boxes in all for the run.
    let mut w = W::new();
    let leader = [
        ("out_unblock", 1),
        ("in_csp_message", 2),
        ("out_box_job", 2),
    ];
    assert_eq!(w.count("end", &leader), None, "the leader's own line: S5b");
    w.t += 11 * MINUTE;
    assert_eq!(
        w.count("running", &[("out_box_job", 4)]),
        None,
        "2 + 4 of 9"
    );
    assert_eq!(
        w.count("running", &[("out_box_job", 7)]),
        None,
        "2 + 7 of 9"
    );
    assert_eq!(
        w.count("running", &[("out_box_job", 8)]),
        some("follower-wrote"),
        "2 + 8 is past 9"
    );
    // Running lines of one socket do not add up: they are one socket's counters, read again.
    let mut w = W::new();
    assert_eq!(w.count("running", &[("out_box_job", 2)]), None);
    assert_eq!(w.count("running", &[("out_box_job", 3)]), None);
    // The end of a follower socket spends the budget: the next one has what is left (none here).
    assert_eq!(w.count("end", &[("out_box_job", 3)]), None);
    w.t += 11 * MINUTE;
    assert_eq!(
        w.count("running", &[("out_box_job", 1)]),
        some("follower-wrote"),
        "3 earlier + 1 of 3"
    );
    // Slack the leader sockets spent is no halt for a follower that writes no box (three sockets, each
    // within its own bound of three boxes at no incoming message).
    let mut w = W::new();
    for _ in 0..3 {
        assert_eq!(
            w.count("end", &[("out_unblock", 1), ("out_box_job", 3)]),
            None
        );
        w.t += 11 * MINUTE;
    }
    assert_eq!(w.count("running", &[("out_reflected_ack", 5)]), None);
    assert_eq!(
        w.count("running", &[("out_box_job", 1)]),
        some("follower-wrote")
    );
}

/// MEDIUM-1: the blind judge. An account that is online and has said no count line for the bound is a
/// run nobody judges: the table of the pure answer. The caller asks it in a real place only.
#[test]
fn a_judge_that_gets_no_count_line_is_blind_after_the_bound() {
    let bound = u64::try_from(COUNT_LINES_WITHIN.as_millis()).expect("ms");
    // The backend's period is 60 s, so the bound is two periods and a margin.
    let lines =
        std::fs::read_to_string(crate::root().join("crates/kourier-threema/src/conn/lines.rs"))
            .expect("the backend's lines");
    assert!(
        lines.contains("COUNT_EVERY: Duration = Duration::from_secs(60)"),
        "the backend's period changed: the bound is 150 s for 60"
    );
    assert_eq!(bound, 150_000);
    let blind = |w: &W, at: u64| count_lines_missing(&w.j, at, bound);
    // Never online: nothing is owed (the link, a reconnect, a rehearsal's offline account).
    let w = W::new();
    assert_eq!(blind(&w, w.t + 100 * bound), None);
    // Online at t: the line is owed from then on.
    let mut w = W::new();
    let t0 = w.t;
    assert_eq!(w.account("active", "online"), None);
    assert_eq!(blind(&w, t0 + bound - 1), None);
    assert_eq!(blind(&w, t0 + bound), some("count-lines-missing"));
    // A count line, running or the socket's end, renews the wait from its own time.
    w.t = t0 + 100_000;
    assert_eq!(w.count("running", &[]), None);
    assert_eq!(blind(&w, t0 + 100_000 + bound - 1), None);
    assert_eq!(blind(&w, t0 + 100_000 + bound), some("count-lines-missing"));
    w.t = t0 + 200_000;
    assert_eq!(w.count("end", &[]), None);
    assert_eq!(blind(&w, t0 + 200_000 + bound - 1), None);
    // An account that is not online owes nothing; coming back online starts the wait anew.
    w.t = t0 + 300_000;
    assert_eq!(w.account("active", "connecting"), None);
    assert_eq!(blind(&w, t0 + 300_000 + 100 * bound), None);
    w.t = t0 + 400_000;
    assert_eq!(w.account("active", "online"), None);
    assert_eq!(blind(&w, t0 + 400_000 + bound - 1), None);
    assert_eq!(blind(&w, t0 + 400_000 + bound), some("count-lines-missing"));
    // A second `online` event while online does not move the start of the wait.
    w.t = t0 + 450_000;
    assert_eq!(w.account("active", "online"), None);
    assert_eq!(blind(&w, t0 + 400_000 + bound), some("count-lines-missing"));
}

/// LOW-9: the removal the step asked for excuses a 4113, a `logged_out` account and the drop frame
/// only while it stands; given back (an end step failed), each halts again.
#[test]
fn a_removal_given_back_excuses_nothing() {
    let mut w = W::removal();
    assert_eq!(
        w.info("the mediator closed the connection code=4113 meaning=\"Removed\""),
        None
    );
    assert_eq!(w.account("logged_out", "logged_out"), None);
    assert_eq!(w.count("end", &[("out_drop", 1)]), None);
    w.j.removal_asked = false;
    assert_eq!(
        w.info("the mediator closed the connection code=4113 meaning=\"Removed\""),
        some("close-4113")
    );
    assert_eq!(w.account("logged_out", "logged_out"), some("logged-out"));
    assert_eq!(
        W::new().count("end", &[("out_drop", 1)]),
        some("follower-wrote")
    );
}

/// The no-send rule's only opening: a send step's window, on a leader or a follower socket, bounded,
/// and for as long as the step stands.
#[test]
fn a_transmit_frame_passes_only_inside_a_send_window_and_within_its_bounds() {
    let frames = |r: u64, b: u64| {
        vec![
            ("out_reflect_transmit", r),
            ("out_box_transmit", b),
            ("out_unblock", 0),
        ]
    };
    let some = |w: &str| Some(w.to_owned());
    let open = || {
        let mut w = W::new();
        w.j.send.open();
        w
    };
    // No window: the old rule.
    assert_eq!(W::new().count("running", &frames(1, 0)), some("transmit"));
    assert_eq!(W::new().count("running", &frames(0, 1)), some("transmit"));
    // Inside it, up to the bounds and not one frame more of either.
    let mut w = open();
    assert_eq!(w.count("running", &frames(1, 1)), None);
    assert_eq!(w.count("running", &frames(SEND_REFLECTS, SEND_BOXES)), None);
    assert_eq!(
        open().count("running", &frames(SEND_REFLECTS + 1, 1)),
        some("transmit")
    );
    assert_eq!(
        open().count("running", &frames(1, SEND_BOXES + 1)),
        some("transmit")
    );
    // Closed: what the step wrote stays allowed (the counts are cumulative), nothing further is.
    let mut w = open();
    assert_eq!(w.count("running", &frames(1, 2)), None);
    w.j.send.close();
    assert_eq!(w.count("running", &frames(1, 2)), None);
    assert_eq!(w.count("running", &frames(1, 3)), some("transmit"));
    // A second call of a paused step keeps the first call's bounds.
    let mut w = open();
    assert_eq!(w.count("running", &frames(1, 1)), None);
    w.j.send.open();
    assert_eq!(
        w.count("running", &frames(SEND_REFLECTS + 1, 1)),
        some("transmit")
    );
    // The next step adds its own bounds to what the socket has written.
    let mut w = open();
    assert_eq!(w.count("running", &frames(1, 1)), None);
    w.j.send.close();
    w.j.send.open();
    assert_eq!(
        w.count("running", &frames(1 + SEND_REFLECTS, 1 + SEND_BOXES)),
        None
    );
    assert_eq!(
        w.count("running", &frames(2 + SEND_REFLECTS, 1)),
        some("transmit")
    );
    // A socket's end starts the counters at zero: inside the window the bounds start over, after it
    // nothing is allowed.
    let mut w = open();
    assert_eq!(w.count("end", &frames(1, 2)), None);
    assert_eq!(w.count("running", &frames(SEND_REFLECTS, SEND_BOXES)), None);
    w.j.send.close();
    assert_eq!(w.count("end", &frames(SEND_REFLECTS, SEND_BOXES)), None);
    assert_eq!(w.count("running", &frames(0, 1)), some("transmit"));
}

#[test]
fn a_steps_own_send_is_claimed_once_and_given_back_only_when_nothing_went() {
    let mut window = crate::threema_run::judge::SendWindow::default();
    assert!(window.claim("send"));
    assert!(!window.claim("send"));
    assert!(window.claim("group-send"));
    window.give_back("send");
    assert!(window.claim("send"));
    assert!(!window.claim("group-send"));
}
