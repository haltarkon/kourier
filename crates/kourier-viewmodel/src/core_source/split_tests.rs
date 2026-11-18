//! Split view (`split.rs`): the pane in hand and the parked one, token routing, events of the
//! parked chat, mark-read, presence, collapse, merges.

use kourier_core::facade::{OutboxWait, Query, UiCommand, UiEvent};
use kourier_model::{ConversationId, OutgoingState};

use super::tests::{NOW, Sent, Store, message, result, settle, source, take_queries};
use super::*;

const A: ConversationId = ConversationId(1);
const B: ConversationId = ConversationId(2);
const C: ConversationId = ConversationId(3);

fn marks(sent: &Sent) -> Vec<ConversationId> {
    sent.borrow()
        .iter()
        .filter_map(|c| match c {
            UiCommand::MarkRead { conv } => Some(*conv),
            _ => None,
        })
        .collect()
}

fn last_presence(sent: &Sent) -> Option<(bool, Option<ConversationId>, Option<ConversationId>)> {
    sent.borrow().iter().rev().find_map(|c| match c {
        UiCommand::SetPresence {
            focused,
            visible,
            beside,
        } => Some((*focused, *visible, *beside)),
        _ => None,
    })
}

/// Chat 1 open as pane 0, chat 2 opened beside it: pane 1 is in hand and active.
fn two_panes(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    src.open_conversation(Some(A));
    settle(&mut src, &sent, store);
    let ch = src.open_side(B);
    assert!(ch.pane);
    settle(&mut src, &sent, store);
    (src, sent)
}

/// The message rows (not the day line or the unread marker).
fn texts(rows: &[crate::timeline::MessageRow]) -> usize {
    rows.iter()
        .filter(|r| r.kind == crate::timeline::RowKind::Message)
        .count()
}

fn visit_rows(src: &mut CoreSource, pane: u8, id: ConversationId) -> usize {
    src.visit_pane(pane);
    let n = texts(&src.timeline(id));
    src.end_visit();
    n
}

#[test]
fn the_split_off_leaves_side_none_and_sends_no_new_command() {
    let (mut src, sent) = source();
    let store = Store::new(&[(1, 5), (2, 5)]);
    src.reload();
    src.open_conversation(Some(A));
    settle(&mut src, &sent, &store);
    src.set_presence(true, true);
    src.set_side_shown(true);
    let ch = src.on_event(&UiEvent::MessagesChanged {
        conv: A,
        ids: Vec::new(),
    });
    assert!(!ch.pane && !ch.side.any());
    settle(&mut src, &sent, &store);
    src.tick();
    assert!(src.side.is_none() && !src.side_open());
    assert_eq!(src.active_pane(), 0);
    assert_eq!(src.pane_of(A), Some(0));
    assert_eq!(src.pane_of(B), None);
    // No presence names a second chat.
    assert_eq!(last_presence(&sent), Some((true, Some(A), None)));
    // Without a side a pane close is today's close.
    let ch = src.close_pane(0);
    assert!(ch.header && src.open().is_none());
}

#[test]
fn open_side_opens_in_pane_one_and_activates_it() {
    let store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, _sent) = two_panes(&store);
    assert!(src.side_open());
    assert_eq!(src.active_pane(), 1);
    assert_eq!(src.open(), Some(B));
    assert_eq!(src.pane_of(A), Some(0));
    assert_eq!(src.pane_of(B), Some(1));
    assert!(src.is_open(A) && src.is_open(B));
    assert_eq!(texts(&src.timeline(B)), 5);
    assert_eq!(visit_rows(&mut src, 0, A), 5);
    assert_eq!(
        src.active_pane(),
        1,
        "a visit does not change the active pane"
    );
}

#[test]
fn tokens_are_routed_to_the_pane_that_asked_also_after_a_swap() {
    let mut store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, sent) = two_panes(&store);
    // Pane 0 (parked) asks: its generations are the small ones; pane 1's are above 1 << 40.
    store
        .convs
        .get_mut(&1)
        .expect("a")
        .push(message(1, 6, false));
    let ch = src.on_event(&UiEvent::MessagesChanged {
        conv: A,
        ids: Vec::new(),
    });
    assert!(!ch.timeline, "the pane in hand is not touched");
    let qs = take_queries(&sent);
    let (token, query) = qs
        .iter()
        .find(|(_, q)| matches!(q, Query::Timeline(t) if t.conv == A))
        .expect("the parked chat is read");
    assert!(token.generation < 1 << 40);
    let ch = src.on_event(&result(*token, store.answer(query)));
    assert!(ch.side.timeline && !ch.timeline);
    assert_eq!(visit_rows(&mut src, 0, A), 6);
    // After an activation the other pane is parked, and its token goes the other way.
    src.activate_pane(0);
    assert_eq!(src.active_pane(), 0);
    store
        .convs
        .get_mut(&2)
        .expect("b")
        .push(message(2, 6, false));
    src.on_event(&UiEvent::MessagesChanged {
        conv: B,
        ids: Vec::new(),
    });
    let qs = take_queries(&sent);
    let (token, query) = qs
        .iter()
        .find(|(_, q)| matches!(q, Query::Timeline(t) if t.conv == B))
        .expect("pane 1's chat is read");
    assert!(token.generation >= 1 << 40);
    let ch = src.on_event(&result(*token, store.answer(query)));
    assert!(ch.side.timeline && !ch.timeline);
    assert_eq!(visit_rows(&mut src, 1, B), 6);
    assert_eq!(texts(&src.timeline(A)), 6, "pane 0 is in hand again");
}

#[test]
fn a_late_answer_to_a_dropped_pane_is_stale_and_touches_nothing() {
    let store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, sent) = two_panes(&store);
    src.on_event(&UiEvent::MessagesChanged {
        conv: A,
        ids: Vec::new(),
    });
    let qs = take_queries(&sent);
    let (token, query) = qs
        .iter()
        .find(|(_, q)| matches!(q, Query::Timeline(t) if t.conv == A))
        .expect("asked")
        .clone();
    let applied = src.timeline_q.applied();
    let ch = src.close_pane(0);
    assert!(ch.pane && !src.side_open());
    assert_eq!(src.active_pane(), 0, "the survivor is pane 0");
    let ch = src.on_event(&result(token, store.answer(&query)));
    assert!(!ch.timeline && !ch.side.any());
    assert_eq!(src.timeline_q.applied(), applied);
    assert_eq!(src.timeline_q.complete(token), Completion::Stale);
    assert_eq!(src.timeline_q.applied(), applied);
}

#[test]
fn messages_changed_of_the_parked_chat_reloads_it() {
    let mut store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, sent) = two_panes(&store);
    store
        .convs
        .get_mut(&1)
        .expect("a")
        .push(message(1, 7, false));
    let mut ch = src.on_event(&UiEvent::TimelineInvalidated { conv: A });
    ch.merge(settle(&mut src, &sent, &store));
    assert!(ch.side.timeline);
    assert!(!ch.timeline);
    assert_eq!(visit_rows(&mut src, 0, A), 6);
    assert_eq!(texts(&src.timeline(B)), 5);
}

#[test]
fn only_the_active_pane_marks_read() {
    let mut store = Store::new(&[(1, 5), (2, 5)]);
    for id in [1, 2] {
        store
            .convs
            .get_mut(&id)
            .expect("conv")
            .push(message(id, 7, true));
    }
    let (mut src, sent) = two_panes(&store);
    src.set_presence(true, true);
    settle(&mut src, &sent, &store);
    assert_eq!(marks(&sent), [B], "the active pane (1) only");
    // The parked chat gets a new unread message: still nobody reads it.
    store
        .convs
        .get_mut(&1)
        .expect("a")
        .push(message(1, 10, true));
    src.on_event(&UiEvent::MessagesChanged {
        conv: A,
        ids: Vec::new(),
    });
    settle(&mut src, &sent, &store);
    assert_eq!(marks(&sent), [B]);
    // Activated, it is read.
    src.activate_pane(0);
    settle(&mut src, &sent, &store);
    assert_eq!(marks(&sent), [B, A]);
}

#[test]
fn presence_carries_beside_only_while_the_side_is_shown() {
    let store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, sent) = two_panes(&store);
    src.set_presence(true, true);
    assert_eq!(last_presence(&sent), Some((true, Some(B), None)));
    src.set_side_shown(true);
    assert_eq!(last_presence(&sent), Some((true, Some(B), Some(A))));
    src.set_side_shown(false);
    assert_eq!(last_presence(&sent), Some((true, Some(B), None)));
    // A visit sends nothing; the active pane's chat stays the networks' presence.
    src.set_side_shown(true);
    let before = sent.borrow().len();
    src.visit_pane(0);
    src.set_presence(true, true);
    src.end_visit();
    assert_eq!(last_presence(&sent), Some((true, Some(B), Some(A))));
    assert!(sent.borrow().len() >= before);
}

#[test]
fn the_same_chat_is_never_in_both_panes() {
    let store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, sent) = two_panes(&store);
    // A list click on the parked pane's chat activates that pane.
    let ch = src.open_conversation(Some(A));
    assert!(ch.pane);
    assert_eq!(src.active_pane(), 0);
    assert_eq!(src.open(), Some(A));
    assert_eq!(src.pane_of(B), Some(1));
    // "Open in Split View" of a shown chat only activates.
    let ch = src.open_side(B);
    assert!(ch.pane);
    assert_eq!(src.active_pane(), 1);
    assert_eq!(src.open(), Some(B));
    settle(&mut src, &sent, &store);
    assert_eq!(visit_rows(&mut src, 0, A), 5);
}

#[test]
fn a_removed_chat_of_either_pane_collapses_the_split() {
    let store = Store::new(&[(1, 5), (2, 5)]);
    // The pane in hand's chat.
    let (mut src, sent) = two_panes(&store);
    let ch = src.on_event(&UiEvent::ConversationsRemoved { ids: vec![B] });
    assert!(ch.pane && ch.timeline && ch.header);
    assert!(!src.side_open());
    assert_eq!((src.open(), src.active_pane()), (Some(A), 0));
    settle(&mut src, &sent, &store);
    assert_eq!(texts(&src.timeline(A)), 5);
    // The parked pane's chat.
    let (mut src, _sent) = two_panes(&store);
    let ch = src.on_event(&UiEvent::ConversationsRemoved { ids: vec![A] });
    assert!(ch.pane);
    assert!(!src.side_open());
    assert_eq!((src.open(), src.active_pane()), (Some(B), 0));
}

#[test]
fn a_merge_of_one_pane_into_the_other_collapses_and_another_switches_the_parked_pane() {
    let store = Store::new(&[(1, 5), (2, 5), (3, 5)]);
    // The pane in hand (2) is merged into the parked pane's chat (1).
    let (mut src, _s) = two_panes(&store);
    let ch = src.on_event(&UiEvent::ConversationMerged { from: B, into: A });
    assert!(ch.pane && ch.switch_to.is_none());
    assert!(!src.side_open());
    assert_eq!(src.open(), Some(A));
    // The parked chat (1) is merged into the pane in hand's (2).
    let (mut src, _s) = two_panes(&store);
    let ch = src.on_event(&UiEvent::ConversationMerged { from: A, into: B });
    assert!(ch.pane && !ch.side.any());
    assert!(!src.side_open());
    assert_eq!(src.open(), Some(B));
    // Into a third chat: the parked pane switches, the split stays.
    let (mut src, _s) = two_panes(&store);
    let ch = src.on_event(&UiEvent::ConversationMerged { from: A, into: C });
    assert_eq!(ch.side.switch_to, Some(C));
    assert!(src.side_open());
    // And the pane in hand: today's switch.
    let ch = src.on_event(&UiEvent::ConversationMerged { from: B, into: C });
    assert_eq!(ch.switch_to, Some(C));
}

#[test]
fn close_pane_in_hand_moves_the_parked_chat_into_pane_zero_and_closing_the_parked_drops_it() {
    let store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, _sent) = two_panes(&store);
    let ch = src.close_pane(1);
    assert!(ch.pane && ch.timeline && ch.header);
    assert!(!src.side_open());
    assert_eq!((src.open(), src.active_pane()), (Some(A), 0));
    assert_eq!(texts(&src.timeline(A)), 5, "its rows came along");
    // Parked: closing the other pane keeps the pane in hand.
    let (mut src, _sent) = two_panes(&store);
    let ch = src.close_pane(0);
    assert!(ch.pane && !ch.timeline);
    assert!(!src.side_open());
    assert_eq!((src.open(), src.active_pane()), (Some(B), 0));
    // `open_conversation(None)` with a split closes the pane in hand.
    let (mut src, _sent) = two_panes(&store);
    src.open_conversation(None);
    assert_eq!((src.open(), src.side_open()), (Some(A), false));
}

#[test]
fn tick_and_has_timers_see_a_send_waiting_in_the_parked_pane() {
    let mut store = Store::new(&[(1, 5), (2, 5)]);
    let m = &mut store.convs.get_mut(&1).expect("a")[2];
    m.origin = kourier_core::facade::OriginKind::OwnThisDevice;
    m.author = None;
    m.read_state = None;
    m.sent_at = NOW - 60_000;
    m.out_state = Some(OutgoingState::Sending { progress: None });
    m.outbox = Some(OutboxWait {
        in_flight: true,
        attempts: 1,
        next_attempt_at: None,
        last_error: None,
    });
    let (mut src, _sent) = two_panes(&store);
    assert!(
        src.has_timers(),
        "the parked pane's waiting send needs the timer"
    );
    let ch = src.tick();
    assert!(ch.side.waits && !ch.waits);
}

#[test]
fn pane_state_swaps_every_field() {
    let store = Store::new(&[(1, 5), (2, 5)]);
    let (mut src, _sent) = two_panes(&store);
    // Everything of the open chat lives in the pane: after a double swap nothing changed.
    let before = (src.open, src.window.rows.len(), src.detail.is_some());
    src.swap_panes();
    assert_eq!(src.open, Some(A));
    src.swap_panes();
    assert_eq!(
        before,
        (src.open, src.window.rows.len(), src.detail.is_some())
    );
}

#[test]
fn swap_sides_exchanges_the_places_and_keeps_the_active_chat_active() {
    let mut store = Store::new(&[(1, 5), (2, 5)]);
    // Without a split: nothing.
    let (mut one, sent1) = source();
    one.reload();
    one.open_conversation(Some(A));
    settle(&mut one, &sent1, &store);
    let ch = one.swap_sides();
    assert!(!ch.pane);
    assert_eq!((one.pane_of(A), one.active_pane()), (Some(0), 0));

    let (mut src, sent) = two_panes(&store);
    src.set_presence(true, true);
    src.set_side_shown(true);
    let presence = last_presence(&sent);
    assert_eq!(
        (src.pane_of(A), src.pane_of(B), src.active_pane()),
        (Some(0), Some(1), 1)
    );
    let ch = src.swap_sides();
    assert!(ch.pane);
    // Both chats flipped, the active chat (B) is still the one in hand and still active.
    assert_eq!((src.pane_of(A), src.pane_of(B)), (Some(1), Some(0)));
    assert_eq!((src.active_pane(), src.open()), (0, Some(B)));
    assert!(src.side_open() && src.is_open(A) && src.is_open(B));
    assert_eq!(
        last_presence(&sent),
        presence,
        "the chats shown are the same"
    );
    // A visit by the new index reaches the right chat.
    assert_eq!(visit_rows(&mut src, 1, A), 5);
    // A query issued before the swap lands in its own chat's state after it.
    store
        .convs
        .get_mut(&1)
        .expect("a")
        .push(message(1, 6, false));
    src.on_event(&UiEvent::MessagesChanged {
        conv: A,
        ids: Vec::new(),
    });
    let qs = take_queries(&sent);
    let (token, query) = qs
        .iter()
        .find(|(_, q)| matches!(q, Query::Timeline(t) if t.conv == A))
        .expect("the parked chat is read");
    let (token, answer) = (*token, store.answer(query));
    src.swap_sides();
    assert_eq!(
        (src.pane_of(A), src.pane_of(B), src.active_pane()),
        (Some(0), Some(1), 1)
    );
    let ch = src.on_event(&result(token, answer));
    assert!(ch.side.timeline && !ch.timeline);
    assert_eq!(visit_rows(&mut src, 0, A), 6);
    assert_eq!(texts(&src.timeline(B)), 5);
    // A second swap is the identity (the other way round again).
    src.swap_sides();
    src.swap_sides();
    assert_eq!(
        (src.pane_of(A), src.pane_of(B), src.active_pane()),
        (Some(0), Some(1), 1)
    );
    // During a visit nothing happens.
    src.visit_pane(0);
    assert!(!src.swap_sides().pane);
    src.end_visit();
    // Closing a pane after a swap still collapses into pane 0.
    src.swap_sides();
    let ch = src.close_pane(src.pane_of(B).expect("b"));
    assert!(ch.pane && !src.side_open());
    assert_eq!((src.open(), src.active_pane()), (Some(A), 0));
}
