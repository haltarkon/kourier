//! The call line's merge rule: the signals of one call, in any order and any number of times,
//! with the clock passing a ring's deadline at any moment in between, end in the same line.
//! And who may say what: with signals of both origins in every order, what the other side of a
//! call is allowed to say never moves a line that one of the user's own devices settled.

use kourier_model::{CallDirection, CallLine, CallMedia, CallState, UserId};
use proptest::prelude::*;

/// The instant after every deadline and every signal of the strategies below.
const END: i64 = 1_000_000;

fn state() -> impl Strategy<Value = CallState> {
    prop_oneof![
        Just(CallState::Started),
        (0..END).prop_map(|deadline| CallState::Ringing { deadline }),
        Just(CallState::Missed),
        Just(CallState::Unanswered),
        Just(CallState::Declined),
        Just(CallState::Answered),
    ]
}

/// Any signal, also ones no backend would send (an outgoing ring, a group flag that differs
/// between two signals of one call): the rule holds for all of them.
fn signal() -> impl Strategy<Value = CallLine> {
    (
        prop_oneof![Just(CallDirection::Incoming), Just(CallDirection::Outgoing)],
        any::<bool>(),
        state(),
        prop::option::of(prop_oneof![Just(CallMedia::Voice), Just(CallMedia::Video)]),
        prop::option::of(prop::sample::select(vec!["aci:a", "aci:b", "aci:c"])),
        // Few distinct instants, so two signals often carry the same one.
        0..6i64,
    )
        .prop_map(|(direction, group, state, media, by, at)| {
            let mut line = CallLine::signal(direction, group, state, at);
            if let Some(m) = media {
                line = line.with_media(m, at);
            }
            if let Some(u) = by {
                line = line.with_by(UserId::parse(u).expect("user id"), at);
            }
            line
        })
}

/// The signals folded in the order given, the clock read (`settled`) wherever `clock` says,
/// at instants that only grow; then read once more at the end.
fn fold(signals: &[CallLine], clock: &[Option<i64>]) -> CallLine {
    let mut now = 0;
    let mut line: Option<CallLine> = None;
    for (i, s) in signals.iter().enumerate() {
        let joined = match &line {
            Some(l) => l.join(s),
            None => s.clone().normalized(),
        };
        line = Some(match clock.get(i).copied().flatten() {
            Some(t) => {
                now = now.max(t);
                joined.settled(now)
            }
            None => joined,
        });
    }
    line.expect("at least one signal").settled(END)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn any_order_of_any_signals_gives_the_same_line(
        (signals, order, dups, clock_a, clock_b) in prop::collection::vec(signal(), 1..7)
            .prop_flat_map(|signals| {
                let n = signals.len();
                (
                    Just(signals),
                    Just((0..n).collect::<Vec<_>>()).prop_shuffle(),
                    prop::collection::vec((0..n, 0..=n), 0..4),
                    prop::collection::vec(prop::option::of(0..END), 0..12),
                    prop::collection::vec(prop::option::of(0..END), 0..12),
                )
            })
    ) {
        let expected = fold(&signals, &clock_a);
        let mut shuffled: Vec<CallLine> = order.iter().map(|i| signals[*i].clone()).collect();
        for (src, at) in dups {
            let copy = shuffled[src % shuffled.len()].clone();
            shuffled.insert(at.min(shuffled.len()), copy);
        }
        let got = fold(&shuffled, &clock_b);
        prop_assert_eq!(&got, &expected, "order {:?}", order);
        // The line is in its one form, and joining it with itself or with any of its
        // signals changes nothing: a line never moves down.
        prop_assert_eq!(&got.clone().normalized(), &got);
        prop_assert_eq!(&got.join(&got), &got);
        for s in &signals {
            prop_assert_eq!(&got.join(s).settled(END), &got);
            prop_assert!(got.state.rank() >= s.settled(END).state.rank());
        }
    }

    /// Signals of both origins in any order. The core takes from the other side only what
    /// [`CallLine::may_come_from_peer`] allows and keeps every ring on its own clock, capped
    /// ([`CallLine::on_local_clock`]). Then: any order gives the same line; no line rings
    /// longer than the cap after the receipt of its ring; and once the user's own devices
    /// settled the call (their signals alone end in something other than "started"), nothing
    /// the other side says changes its state or its direction.
    #[test]
    fn the_other_side_never_moves_what_an_own_device_settled(
        (signals, order) in prop::collection::vec((signal(), any::<bool>(), 0..50i64), 1..8)
            .prop_flat_map(|signals| {
                let n = signals.len();
                (Just(signals), Just((0..n).collect::<Vec<_>>()).prop_shuffle())
            })
    ) {
        // What the core ingests of one signal made at its `media_at`/`by_at` instant and
        // received `delay` later: `None` when the other side said more than it may.
        let ingested = |(line, from_peer, delay): &(CallLine, bool, i64)| -> Option<CallLine> {
            if *from_peer && !line.may_come_from_peer() {
                return None;
            }
            let made_at = line.media_at.max(line.by_at);
            Some(line.clone().on_local_clock(made_at, made_at + delay))
        };
        let own: Vec<CallLine> = signals
            .iter()
            .filter(|s| !s.1)
            .filter_map(ingested)
            .collect();
        let all: Vec<CallLine> = signals.iter().filter_map(ingested).collect();
        let shuffled: Vec<CallLine> = order
            .iter()
            .filter_map(|i| ingested(&signals[*i]))
            .collect();
        for line in &all {
            if let Some(deadline) = line.deadline() {
                prop_assert!(deadline <= 5 + 50 + kourier_model::MAX_RING_MS);
            }
        }
        if all.is_empty() {
            return Ok(());
        }
        let end = END + kourier_model::MAX_RING_MS;
        let whole = fold(&all, &[]).settled(end);
        prop_assert_eq!(&fold(&shuffled, &[]).settled(end), &whole, "order {:?}", order);
        if !own.is_empty() {
            let settled = fold(&own, &[]).settled(end);
            if settled.state != CallState::Started {
                prop_assert_eq!(whole.state, settled.state);
                prop_assert_eq!(whole.direction, settled.direction);
            }
        }
    }

    #[test]
    fn the_join_is_commutative_and_associative(a in signal(), b in signal(), c in signal()) {
        prop_assert_eq!(a.join(&b), b.join(&a));
        prop_assert_eq!(a.join(&b).join(&c), a.join(&b.join(&c)));
        prop_assert_eq!(a.join(&a), a.clone().normalized());
    }
}

/// The table of the design, signal by signal.
#[test]
fn the_states_have_one_order() {
    use CallState::{Answered, Declined, Missed, Ringing, Started, Unanswered};
    let ranks: Vec<u8> = [
        Started,
        Ringing { deadline: 0 },
        Missed,
        Unanswered,
        Declined,
        Answered,
    ]
    .into_iter()
    .map(CallState::rank)
    .collect();
    assert_eq!(ranks, [0, 1, 2, 2, 3, 4]);
}
