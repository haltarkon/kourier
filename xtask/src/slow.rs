//! T5d: the slow set. Tests that cost seconds to minutes and guard durability or a long wait:
//! `ci --quick` leaves them out and says so; only the full `cargo xtask ci` (and so a landing,
//! which is a full ci) runs them. `cargo xtask dist` runs no tests at all. The full run has no
//! filter, so it needs nothing from this list but the unit tests below.
//!
//! The list is the user's choice of 2026-10-15 (`target/test-load/decisions.md`, "Slow
//! checks: Before each commit only"): the eight kill-at-every-failpoint loops (overlap.md B),
//! the 5,000-message ingest rate, and the long transfer deadline and back-off tests (overlap.md
//! C3, C4: real time on the Node mock, no clock to inject). The exhaustive any-order tests
//! (`calls_attack` direct and group calls, `history_rules_review` converge in any order) join it as
//! their full forms once each has a sample twin in the regular set. A test is named as nextest
//! names it, `<test file>::<fn>` (`crash::kill_at_every_point_…`), and by its package, because
//! two crates can have a file of one name. It is a LIST IN XTASK and not a nextest profile
//! because the full run after a green quick one needs the same text as its complement
//! ([`crate::green::remainder`]).
//!
//! The window tests of `kourier-qt` are binaries without libtest's harness: each answers nextest's
//! listing with the names of its tests (`tests/support/mod.rs` `answer_list_of`), mostly its own name.
//! `scroll_ui` lists two (SE6, 2026-10-26): `scroll_ui` and `scroll_ui_climbed`, the second the very
//! session with the exact distance of the wheel step switched on, which flapped once under load. A
//! Qt entry is named by its package and that listed name (no `<file>::`); quick's Qt step leaves it
//! out ([`window_filter`]), the full run takes it, and the full run after a quick run that did the
//! window tests takes it alone ([`window_slow_filter`]).

/// One slow test.
pub struct SlowTest {
    pub package: &'static str,
    /// `<test file>::<fn>`, the name nextest prints.
    pub test: &'static str,
    /// Seconds of one run, quiet / loaded (`target/test-load/overlap.md`) and what the test
    /// guards, for whoever wonders why it is here.
    #[allow(
        dead_code,
        reason = "documentation: read by whoever opens the list, and by a test"
    )]
    pub why: &'static str,
}

pub const SLOW_TESTS: [SlowTest; 30] = [
    SlowTest {
        package: "kourier-core",
        test: "crash::kill_at_every_point_gives_the_uncrashed_state",
        why: "33 / 76 s: kill at every failpoint of the write path (7 failpoints x every hit count)",
    },
    SlowTest {
        package: "kourier-core",
        test: "attachments_crash::kills_anywhere_in_the_attachment_pipeline_give_the_uncrashed_state",
        why: "28 / 35 s: the same for the attachment pipeline",
    },
    SlowTest {
        package: "kourier-e2e",
        test: "signal_core::killing_the_app_at_every_receive_failpoint_gives_the_uncrashed_state",
        why: "16 / 18 s: kill the whole app at every receive failpoint",
    },
    SlowTest {
        package: "kourier-signal",
        test: "mock_server::kill_at_every_failpoint_gives_the_uncrashed_state",
        why: "16 / 17 s: kill the Signal actor at every failpoint of the mock-server path",
    },
    SlowTest {
        package: "kourier-e2e",
        test: "signal_send::a_read_reaches_the_phone_across_a_kill",
        why: "14 / 18 s: the read sync is on the wire once across a kill",
    },
    SlowTest {
        package: "kourier-e2e",
        test: "signal_modifiers::an_edit_and_a_reaction_are_sent_once_across_a_kill",
        why: "16 / 16 s: an edit and a reaction are sent once across a kill",
    },
    SlowTest {
        package: "kourier-e2e",
        test: "signal_files::a_multi_message_send_survives_a_kill_in_the_middle",
        why: "15 / 15 s: an album and files are sent once across a kill",
    },
    SlowTest {
        package: "kourier-signal",
        test: "history_import::a_kill_at_every_zone_of_the_delivery_goes_on_at_its_cursor_and_appends_every_row_once",
        why: "7 / 15 s: the delivery's cursor and every row exactly once, kill at every zone",
    },
    // W2b (2026-10-16): the five kill loops of the WhatsApp receive path, in a debug build with the
    // child process re-run at every hit of a failpoint. Times: nextest's, in four quick or full runs
    // (they agree within 3 s; a quiet machine has not been measured separately).
    SlowTest {
        package: "kourier-whatsapp",
        test: "wire_kill::every_message_once_after_a_kill_in_the_pending_rows_write_and_cleanup",
        why: "68 s: kill the receive path at every hit of the pending rows' write and cleanup (every message once)",
    },
    SlowTest {
        package: "kourier-whatsapp",
        test: "wire_kill::every_message_once_after_a_kill_in_the_signal_cache_flush",
        why: "27 s: the same for the Signal cache flush",
    },
    SlowTest {
        package: "kourier-whatsapp",
        test: "wire_kill::every_receipt_is_written_after_a_kill_in_the_handlers_group_commit",
        why: "24 s: a kill inside and after the handler's group commit leaves every receipt unacknowledged or durable",
    },
    SlowTest {
        package: "kourier-whatsapp",
        test: "wire_kill::every_message_once_after_a_kill_in_the_inbox_zone",
        why: "18 s: the same as the cache flush's for the inbox zone's commit",
    },
    SlowTest {
        package: "kourier-whatsapp",
        test: "wire_kill::every_message_once_after_a_kill_in_stage_two",
        why: "18 s: the same for stage two",
    },
    SlowTest {
        package: "kourier-e2e",
        test: "signal_core::five_thousand_queued_messages_ingest_at_500_per_second",
        why: "7 / 9 s: the ingest rate (cargo xtask perf judges the same path in release)",
    },
    SlowTest {
        package: "kourier-signal",
        test: "history_transfer::the_wait_for_the_phone_ends_at_its_deadline",
        why: "18 s: a 4 s + 2 s deadline in real time on the Node mock",
    },
    SlowTest {
        package: "kourier-signal",
        test: "history_transfer::a_long_poll_that_is_not_held_is_asked_again_after_a_pause",
        why: "13 s: the real back-off 1, 2, 3 s of the transfer's long poll",
    },
    SlowTest {
        package: "kourier-signal",
        test: "history_transfer::the_time_the_application_was_closed_does_not_count_for_the_download",
        why: "13 s: closed for 9 s against an 8 s bound",
    },
    SlowTest {
        package: "kourier-signal",
        test: "robustness::a_dropped_socket_is_reopened_with_growing_delays_and_at_once_on_a_network_change",
        why: "26 s: the socket's back-off 1, 2, 3, 5 s against a real 503",
    },
    SlowTest {
        package: "kourier-core",
        test: "history_rules_review::review_unread_history_rows_converge_in_any_order_exhaustive",
        why: "10.9 s: all 720 orders of the six deliveries; the regular set runs a sample of 104",
    },
    SlowTest {
        package: "kourier-core",
        test: "calls_attack::every_order_of_every_subset_of_the_signals_of_a_direct_call_exhaustive",
        why: "6.5 s: every order of every subset of a direct call's signals (the sample: 193)",
    },
    SlowTest {
        package: "kourier-core",
        test: "calls_attack::every_order_of_every_subset_of_the_signals_of_a_group_call_exhaustive",
        why: "8.0 s: the same for a group call",
    },
    SlowTest {
        package: "kourier-signal",
        test: "history_import::backend_db_stays_small_while_twenty_thousand_messages_are_delivered",
        why: "6 s: a load flake (1 failure at load ~28, none in 60 loaded runs, 2026-10-16), 20,000 rows delivered",
    },
    SlowTest {
        package: "kourier-signal",
        test: "history_transfer::a_download_that_trickles_past_its_deadline_fails_the_transfer",
        why: "10 s: a load flake (none in 60 loaded runs since its deadline became 8 s, 2026-10-16)",
    },
    SlowTest {
        package: "kourier-e2e",
        test: "signal_history_import::the_re_read_after_a_lost_stage_doubles_nothing",
        why: "3 s: a load flake (one 61 s timeout at load 28, none in 60 loaded runs, 2026-10-16)",
    },
    // W3b (2026-10-17): the two tests of the real WhatsApp client whose first dial missed its own 10-15 s
    // deadline under load (a quick run's 3 test threads beside another session's build). The same two
    // failed in the W2a logs, before any W3 code existed.
    SlowTest {
        package: "kourier-whatsapp",
        test: "actor_real::a_kill_while_a_logout_waits_leaves_the_device_and_the_next_logout_goes_on",
        why: "0.35 / 15 s: load flake (real client's first dial under load, 2026-10-17); a kill while a logout waits leaves the device and the next logout goes on",
    },
    SlowTest {
        package: "kourier-whatsapp",
        test: "actor_real::the_real_client_runs_offline",
        why: "6.7 / 10 s: load flake (real client's first dial under load, 2026-10-17); the real client over guard parts goes offline, tries again and shuts down",
    },
    // W5a (chunk 15a): the real 75 s ack wait (integration tests link the crate without cfg(test)).
    SlowTest {
        package: "kourier-whatsapp",
        test: "send_wire::a_silent_ack_ends_the_connection_and_the_resend_uses_the_same_id_over_the_wire",
        why: "80 s: a frame with no ack ends its connection after the real ACK_WAIT and the resend keeps the id (D5)",
    },
    // W5a (chunk 15b): the send path's kill loop (its one-kill-per-point sample stays in the regular set).
    SlowTest {
        package: "kourier-whatsapp",
        test: "send_kill::every_send_once_after_a_kill_at_every_point",
        why: "17 s: every hit of the send outcome zone and the core's two points around the answer; D21's invariant after each next start",
    },
    // W6a2 (B11): the kill sample of the group half: a start whose batch states a member list releases an admin's delete
    // that waits for it; aborted inside and after the event writer's commits and stage 2's zone, the next start ends in the
    // same place and gives the core the delete once.
    SlowTest {
        package: "kourier-whatsapp",
        test: "groups_flow::an_admins_delete_is_released_once_after_a_kill_around_the_batch_zone",
        why: "12 s: kill the start that states a group's members at every hit of the event writer's and stage 2's commits (the held delete is released and given once)",
    },
    // SE6 (P5c fix round 2, 2026-10-26): the Qt window tests' only slow entry. The step `climbed` of
    // `kourier-qt/tests/scroll_ui.rs` (four notches of the wheel move the view by exactly 240 px, 100-380 with
    // the animation) flapped once under load in a quick run ("moved the view by 178 px", the residual ~2 in 60
    // of the product race fixed in 9f1253f, open in notes/kourier/ui-features.md). The regular `scroll_ui`
    // runs every other check of the session and skips that bound; this entry is the same session with it on.
    SlowTest {
        package: "kourier-qt",
        test: "scroll_ui_climbed",
        why: "28 / 29 s: load flake (the wheel step's exact distance in scroll_ui's `climbed`, ~2 in 60 under eight busy loops, 2026-10-26); the whole session in two runs, smooth and instant, with the bound on",
    },
];

/// How many slow tests each of these `packages` has, for those that have some (sorted by
/// package): what quick leaves out when it runs their integration tests (the Qt crate's window
/// tests too, when quick runs them).
pub fn left_out<'a>(packages: impl IntoIterator<Item = &'a str>) -> Vec<(&'static str, usize)> {
    let wanted: std::collections::BTreeSet<&str> = packages.into_iter().collect();
    let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for t in SLOW_TESTS.iter().filter(|t| wanted.contains(t.package)) {
        *counts.entry(t.package).or_default() += 1;
    }
    counts.into_iter().collect()
}

/// What quick says about the slow tests it did not run (T5 review F3); `None` when it left out
/// none. The reason is what they guard, and who runs them.
pub fn left_out_note(left: &[(&str, usize)]) -> Option<String> {
    if left.is_empty() {
        return None;
    }
    let total: usize = left.iter().map(|(_, n)| n).sum();
    let packages: Vec<String> = left.iter().map(|(p, n)| format!("{p} {n}")).collect();
    Some(format!(
        "the slow set ({total} tests: {}; the kill-at-every-failpoint loops, the 5,000-message rate, \
         the transfer deadlines and the wheel test's exact distance; only the full `cargo xtask ci` \
         runs them)",
        packages.join(", ")
    ))
}

/// The nextest filter that picks exactly `tests`.
fn filter_of<'a>(tests: impl IntoIterator<Item = &'a SlowTest>) -> String {
    tests
        .into_iter()
        .map(|t| format!("(package(={}) & test(={}))", t.package, t.test))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The nextest filter that picks exactly the slow tests.
pub fn filter() -> String {
    filter_of(&SLOW_TESTS)
}

/// The slow tests of the Qt window-test crate.
fn window_tests() -> impl Iterator<Item = &'static SlowTest> {
    SLOW_TESTS
        .iter()
        .filter(|t| crate::quick::needs_offscreen(t.package))
}

/// The filter of the Qt window tests in the quick tier (and in a full run that has to do them all
/// but is told what the slow ones are): every integration test target (`kind(test)`) without the
/// slow entries of the Qt crate (SE6). Without such an entry it is plain `kind(test)`.
pub fn window_filter() -> String {
    let slow = filter_of(window_tests());
    if slow.is_empty() {
        "kind(test)".to_owned()
    } else {
        format!("kind(test) & !({slow})")
    }
}

/// The filter of the Qt window tests' slow entries alone: what the full run still has to do after a
/// quick run that did the other window tests ([`crate::green::remainder`]): `kind(test)` (never a
/// unit test of the same name) and the entries. `None` when there is none.
pub fn window_slow_filter() -> Option<String> {
    let slow = filter_of(window_tests());
    (!slow.is_empty()).then(|| format!("kind(test) & ({slow})"))
}

/// The filter of the quick tier's integration step for the non-Qt `packages`: the integration
/// test binaries of these packages (`kind(test)`, as `runner::integration_select` always had
/// it) without the slow set. The full tier after a green quick run takes the rest of
/// `kind(test)` by negating this very text ([`crate::green::remainder`]).
pub fn integration_filter(packages: &[&str]) -> String {
    let any: Vec<String> = packages.iter().map(|p| format!("package(={p})")).collect();
    format!("kind(test) & ({}) & !({})", any.join(" | "), filter())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_model::{Item, matches};
    use crate::green::{Digest, Ran, Record, Tier, remainder};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn every_slow_test_exists_where_it_is_named() {
        let root = crate::root();
        for t in &SLOW_TESTS {
            let Some((module, name)) = t.test.split_once("::") else {
                // A window test of the Qt crate (no libtest harness): its listed name stands as a
                // string in one of the crate's test files (`answer_list_of`).
                assert!(
                    crate::quick::needs_offscreen(t.package),
                    "{} {}: only the Qt crate's tests are named without `<file>::`",
                    t.package,
                    t.test
                );
                let dir = root.join("crates").join(t.package).join("tests");
                let quoted = format!("\"{}\"", t.test);
                let found = std::fs::read_dir(&dir)
                    .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                    .any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains(&quoted)));
                assert!(
                    found,
                    "no test file of {} lists {}: a renamed slow test falls into the regular set",
                    t.package, t.test
                );
                continue;
            };
            let path = root
                .join("crates")
                .join(t.package)
                .join("tests")
                .join(format!("{module}.rs"));
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                source.contains(&format!("fn {name}(")),
                "{} has no test {name}: a renamed slow test falls into the regular set",
                path.display()
            );
        }
    }

    /// The names of the tests of a Rust test file: the `fn` that follows a `#[test]` or `#[tokio::test…]`
    /// line (other attributes and `async` in between).
    fn test_names(source: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut pending = false;
        for line in source.lines().map(str::trim_start) {
            if line.starts_with("#[test]") || line.starts_with("#[tokio::test") {
                pending = true;
                continue;
            }
            if pending && let Some(rest) = line.split("fn ").nth(1) {
                if let Some((name, _)) = rest.split_once('(') {
                    names.push(name.trim().to_owned());
                }
                pending = false;
            }
        }
        names
    }

    /// Of `tests`, those `is_slow` names and those it does not.
    fn by_tier(tests: &[String], is_slow: impl Fn(&str) -> bool) -> (Vec<&String>, Vec<&String>) {
        tests.iter().partition(|t| is_slow(t))
    }

    /// W2c L4 (review): the wire tests of `kourier-whatsapp/tests/prepair.rs` are the control (a pairing
    /// the rules accept still gets its `pair-device-sign`) and the refusals that mean something only
    /// beside it. `SLOW_TESTS` names all of them or none: a later move of one test to the slow set would
    /// let the quick tier run a refusal without its control, or the control without a refusal. (The
    /// file has no `#[ignore]` either: a source check in `kourier-whatsapp/tests/builder.rs`.)
    /// Mutant: one `prepair::` entry added to `SLOW_TESTS`.
    #[test]
    fn the_pre_pair_wire_tests_stay_in_one_tier() {
        let path = crate::root().join("crates/kourier-whatsapp/tests/prepair.rs");
        let source =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let tests = test_names(&source);
        assert!(
            tests.len() >= 5,
            "the control, the three refusals and the child: {tests:?}"
        );
        let (slow, regular) = by_tier(&tests, |name| {
            SLOW_TESTS
                .iter()
                .any(|t| t.package == "kourier-whatsapp" && t.test == format!("prepair::{name}"))
        });
        assert!(
            slow.is_empty() || regular.is_empty(),
            "tests/prepair.rs is split between the tiers: slow {slow:?}, regular {regular:?}"
        );
    }

    /// The guard above sees a split: a source with two tests of which one is slow is a split, and
    /// the names are read past other attributes and `async`.
    #[test]
    fn the_tier_guard_sees_a_split_and_reads_test_names() {
        let source = "#[test]\nfn one() {}\n\n#[tokio::test(flavor = \"multi_thread\")]\nasync fn two() {}\nfn helper() {}\n#[test]\n#[ignore]\nfn three() {}\n";
        let tests = test_names(source);
        assert_eq!(tests, ["one", "two", "three"]);
        let (slow, regular) = by_tier(&tests, |t| t == "two");
        assert_eq!((slow.len(), regular.len()), (1, 2));
        let (slow, regular) = by_tier(&tests, |_| true);
        assert_eq!((slow.len(), regular.len()), (3, 0));
    }

    #[test]
    fn no_test_is_listed_twice_and_every_entry_says_why() {
        let names: BTreeSet<(&str, &str)> =
            SLOW_TESTS.iter().map(|t| (t.package, t.test)).collect();
        assert_eq!(names.len(), SLOW_TESTS.len());
        assert!(SLOW_TESTS.iter().all(|t| t.why.contains(" s")));
    }

    /// The seconds of a `why` that starts with its run time: `N s: …`, or `N / M s: …` (quiet /
    /// loaded; fractions allowed). `None` when it does not start that way.
    fn run_times(why: &str) -> Option<Vec<f64>> {
        let (times, _) = why.split_once(" s: ")?;
        times
            .split(" / ")
            .map(|t| t.parse::<f64>().ok().filter(|s| *s > 0.0))
            .collect()
    }

    /// Each entry's `why` starts with the seconds of one run, so whoever reads the list sees what a
    /// test costs without opening the test (the five kill loops of the WhatsApp receive path take
    /// 18 to 68 s each, which is why they are here).
    #[test]
    fn every_why_starts_with_its_run_time() {
        for t in &SLOW_TESTS {
            let times = run_times(t.why).unwrap_or_else(|| {
                panic!(
                    "{} {}: the why starts \"N s: \" or \"N / M s: \"",
                    t.package, t.test
                )
            });
            assert!(times.len() <= 2, "{}: {}", t.test, t.why);
        }
        assert_eq!(run_times("33 / 76 s: kill"), Some(vec![33.0, 76.0]));
        assert_eq!(run_times("10.9 s: all"), Some(vec![10.9]));
        for bad in [
            "kill at every point, 33 s",
            "about 7 s: x",
            "s: x",
            " s: x",
            "0 s: x",
            "7 s x",
            "1 / s: x",
        ] {
            assert_eq!(run_times(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_filter_names_each_test_with_its_package() {
        let text = filter();
        assert!(text.starts_with("(package(=kourier-core) & test(=crash::kill_at_every_point"));
        assert_eq!(text.matches(" | ").count(), SLOW_TESTS.len() - 1);
        assert_eq!(text.matches("package(=").count(), SLOW_TESTS.len());
    }

    /// SE6: the Qt window tests' slow entry is left out of quick's Qt step and taken, alone, by the
    /// full run after a quick run that did the other window tests; together the two are every window
    /// test, none twice, and the regular `scroll_ui` stays in quick.
    #[test]
    fn the_slow_window_test_is_left_out_of_quick_and_taken_by_the_remainder_alone() {
        let window = |name| Item {
            package: "kourier-qt",
            name,
            kind: "test",
        };
        let (regular, slow) = (window("scroll_ui"), window("scroll_ui_climbed"));
        let quick = window_filter();
        let rest = window_slow_filter().expect("the Qt crate has a slow entry");
        assert!(
            matches(&quick, &regular) && !matches(&quick, &slow),
            "{quick}"
        );
        assert!(!matches(&rest, &regular) && matches(&rest, &slow), "{rest}");
        // The unit tests of the crate are in neither (they ran in quick's unit step).
        let unit = Item {
            package: "kourier-qt",
            name: "scroll_ui_climbed",
            kind: "lib",
        };
        assert!(!matches(&quick, &unit) && !matches(&rest, &unit));
        // Every Qt entry of the list, and a plain window test beside it, in both worlds.
        let mut universe = vec![window("chat_ui")];
        universe.extend(window_tests().map(|t| window(t.test)));
        for item in &universe {
            let (in_quick, in_rest) = (matches(&quick, item), matches(&rest, item));
            assert!(in_quick || in_rest, "{item:?}");
            assert!(!(in_quick && in_rest), "{item:?} would run twice");
        }
    }

    #[test]
    fn the_quick_filter_is_the_integration_tests_of_its_packages_without_the_slow_ones() {
        assert_eq!(
            integration_filter(&["kourier-app", "kourier-desktop"]),
            format!(
                "kind(test) & (package(=kourier-app) | package(=kourier-desktop)) & !({})",
                filter()
            )
        );
        let item = |package, name, kind| Item {
            package,
            name,
            kind,
        };
        let universe = [
            item(
                "kourier-core",
                "crash::kill_at_every_point_gives_the_uncrashed_state",
                "test",
            ),
            item("kourier-core", "crash::another", "test"),
            item("kourier-core", "unit_test", "lib"),
            // The same file and test name in a package whose list entry is another one.
            item(
                "kourier-signal",
                "crash::kill_at_every_point_gives_the_uncrashed_state",
                "test",
            ),
            item(
                "kourier-e2e",
                "signal_core::five_thousand_queued_messages_ingest_at_500_per_second",
                "test",
            ),
            item("kourier-e2e", "signal_core::another", "test"),
        ];
        let quick = integration_filter(&["kourier-core", "kourier-e2e"]);
        let picked: Vec<&str> = universe
            .iter()
            .filter(|i| matches(&quick, i))
            .map(|i| i.name)
            .collect();
        assert_eq!(picked, ["crash::another", "signal_core::another"]);
    }

    #[test]
    fn the_full_run_after_quick_gets_the_slow_tests_and_the_unreached_packages() {
        let digest = Digest {
            parts: BTreeMap::new(),
            total: 1,
        };
        let records = vec![(
            Tier::Quick,
            Record {
                digest: 1,
                parts: BTreeMap::new(),
                at: 5,
                engine: "nextest".to_owned(),
                ran: Ran {
                    integration: ["kourier-core".to_owned()].into(),
                    ..Ran::default()
                },
            },
        )];
        let rest = remainder(&records, &digest, "nextest", &integration_filter)
            .expect("a remainder")
            .non_qt_filter;
        let quick = integration_filter(&["kourier-core"]);
        // Every integration test of the real list, and a plain one beside each, in both worlds.
        let mut universe = Vec::new();
        for t in &SLOW_TESTS {
            universe.push(Item {
                package: t.package,
                name: t.test,
                kind: "test",
            });
        }
        for package in ["kourier-core", "kourier-signal", "kourier-e2e"] {
            universe.push(Item {
                package,
                name: "plain::test",
                kind: "test",
            });
            universe.push(Item {
                package,
                name: "unit",
                kind: "lib",
            });
        }
        for item in &universe {
            let in_quick = matches(&quick, item);
            let in_rest = matches(&rest, item);
            assert_eq!(in_quick || in_rest, item.kind == "test", "{item:?}");
            assert!(!(in_quick && in_rest), "{item:?} would run twice");
        }
        // The slow tests of a reached package are in the rest, not in quick.
        let slow_core = Item {
            package: "kourier-core",
            name: "crash::kill_at_every_point_gives_the_uncrashed_state",
            kind: "test",
        };
        assert!(!matches(&quick, &slow_core) && matches(&rest, &slow_core));
    }
}
