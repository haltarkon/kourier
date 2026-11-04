//! The sync lane (TG-3 C7a): one request in flight, the read act first and unpaced, paced fetches,
//! a 429 pauses the whole lane for its wait + 10 %.

use kourier_model::CmdId;

use super::*;

const T0: u64 = 1_700_000_000_000;

/// The confirm of a read act of `chat_id` for `cmds` (K4: it also carries the act's `up_to`).
fn read_done(chat_id: i64, cmds: &[i64], up_to: Option<i64>) -> Done {
    Done::Read {
        cmds: cmds.iter().map(|&c| CmdId(c)).collect(),
        chat_id,
        up_to,
    }
}

/// One request in flight: a second waits until the first one's answer came.
#[test]
fn the_sync_lane_holds_one_request() {
    let mut lane = Lane::default();
    lane.push_fetch(Fetch::Chat(-1));
    assert!(lane.push_read(-2, vec![10 << 20], CmdId(1), None).is_none());
    let first = lane.next(T0).expect("the read act goes");
    assert!(matches!(first, LaneAsk::View { chat_id: -2, .. }));
    // Nothing else while it is in flight, at any time.
    assert_eq!(lane.next(T0), None);
    assert_eq!(lane.next(T0 + 60_000), None);
    assert_eq!(lane.due_ms(), None, "no wake while one is in flight");
    assert_eq!(
        lane.on_answer(first, Outcome::Ok, T0 + 5),
        read_done(-2, &[1], None)
    );
    assert_eq!(lane.next(T0 + 5), Some(LaneAsk::Fetch(Fetch::Chat(-1))));
    assert_eq!(lane.waiting(), 0);
}

/// The read act goes AHEAD of the fetches and unpaced (Q-L4); fetches are paced.
#[test]
fn the_read_act_goes_first_and_fetches_are_paced() {
    let mut lane = Lane::default();
    lane.push_fetch(msg(1));
    lane.push_fetch(msg(2));
    let a = lane.next(T0).expect("a fetch");
    assert_eq!(a, LaneAsk::Fetch(msg(1)));
    // A read marker comes while the first fetch runs: it goes right after it.
    assert!(
        lane.push_read(-3, vec![1 << 20, 2 << 20], CmdId(9), None)
            .is_none()
    );
    assert_eq!(lane.on_answer(a, Outcome::Ok, T0 + 10), Done::Fetch(msg(1)));
    assert_eq!(lane.due_ms(), Some(0), "the read act is due at once");
    let view = lane.next(T0 + 10).expect("the read act, unpaced");
    assert_eq!(
        view,
        LaneAsk::View {
            chat_id: -3,
            ids: vec![1 << 20, 2 << 20]
        }
    );
    assert_eq!(
        lane.on_answer(view, Outcome::Ok, T0 + 20),
        read_done(-3, &[9], None)
    );
    // The second fetch waits for the pace (one per FETCH_PACE_MS from the first).
    assert_eq!(lane.due_ms(), Some(T0 + FETCH_PACE_MS));
    assert_eq!(lane.next(T0 + FETCH_PACE_MS - 1), None);
    assert_eq!(lane.next(T0 + FETCH_PACE_MS), Some(LaneAsk::Fetch(msg(2))));
}

/// A paced fetch (`getMessage`).
fn msg(n: i64) -> Fetch {
    Fetch::Message {
        chat_id: -1,
        msg_id: n << 20,
    }
}

/// TF3 (review dF3): `getChat` and `getUser` reach no server (TDLib answers them from memory), so
/// they are NOT paced and go AHEAD of the paced fetches queued before them; still one request in
/// flight. (Mutant: they join the paced FIFO.)
#[test]
fn a_get_chat_is_not_paced_behind_history_pages() {
    let history = |from: i64| Fetch::History {
        chat_id: -2,
        from: from << 20,
        limit: 20,
    };
    let mut lane = Lane::default();
    lane.push_fetch(history(9));
    lane.push_fetch(history(8));
    lane.push_fetch(Fetch::Chat(-3));
    lane.push_fetch(Fetch::User(7));
    assert_eq!(lane.due_ms(), Some(0), "due at once");
    let chat = lane.next(T0).expect("the chat first");
    assert_eq!(chat, LaneAsk::Fetch(Fetch::Chat(-3)));
    // One in flight: nothing else until its answer.
    assert_eq!(lane.next(T0), None);
    lane.on_answer(chat, Outcome::Ok, T0);
    // The user goes at once too, inside the pace window of nothing yet sent.
    assert_eq!(lane.next(T0), Some(LaneAsk::Fetch(Fetch::User(7))));
    lane.on_answer(LaneAsk::Fetch(Fetch::User(7)), Outcome::Ok, T0);
    let page = lane.next(T0).expect("the first page");
    assert_eq!(page, LaneAsk::Fetch(history(9)));
    lane.on_answer(page, Outcome::Ok, T0 + 1);
    // A getChat inside the pace window of the pages goes without waiting for it.
    lane.push_fetch(Fetch::Chat(-4));
    assert_eq!(
        lane.next(T0 + 2),
        Some(LaneAsk::Fetch(Fetch::Chat(-4))),
        "paced behind the history page"
    );
    lane.on_answer(LaneAsk::Fetch(Fetch::Chat(-4)), Outcome::Ok, T0 + 2);
    assert_eq!(lane.next(T0 + 3), None, "the next page waits for its pace");
    assert_eq!(
        lane.next(T0 + FETCH_PACE_MS),
        Some(LaneAsk::Fetch(history(8)))
    );
}

/// A 429 pauses the WHOLE lane for its wait + 10 %, never sooner, and the same request goes again
/// first (the brief §4.5). Pure: the lane counts the clock's ms the actor hands it.
#[test]
fn a_429_pauses_the_sync_lane_for_its_wait() {
    let mut lane = Lane::default();
    lane.push_fetch(Fetch::Chat(-1));
    let a = lane.next(T0).expect("a fetch");
    assert_eq!(
        lane.on_answer(a, Outcome::TooMany(Some(2)), T0 + 100),
        Done::Nothing
    );
    // A read act waits for the pause too (the whole lane waits).
    assert!(lane.push_read(-4, vec![3 << 20], CmdId(2), None).is_none());
    assert_eq!(lane.due_ms(), Some(T0 + 100 + 2_200));
    assert_eq!(
        lane.next(T0 + 100 + 2_199),
        None,
        "sent before the wait + 10 % was over"
    );
    let again = lane.next(T0 + 100 + 2_200).expect("after the wait");
    assert!(matches!(again, LaneAsk::View { chat_id: -4, .. }));
    assert_eq!(
        lane.on_answer(again, Outcome::Ok, T0 + 2_400),
        read_done(-4, &[2], None)
    );
    assert_eq!(
        lane.next(T0 + 2_400),
        Some(LaneAsk::Fetch(Fetch::Chat(-1))),
        "the 429's request goes again"
    );
    // A 429 of a page keeps the page and the read act's command.
    let mut lane = Lane::default();
    lane.push_read(-5, vec![4 << 20], CmdId(3), None);
    let page = lane.next(T0).expect("a page");
    assert_eq!(
        lane.on_answer(page.clone(), Outcome::TooMany(None), T0),
        Done::Nothing
    );
    assert_eq!(lane.next(T0 + 1_099), None);
    assert_eq!(lane.next(T0 + 1_100), Some(page.clone()));
    assert_eq!(
        lane.on_answer(page, Outcome::Ok, T0 + 1_200),
        read_done(-5, &[3], None)
    );
}

/// Pages of 100 ids; a newer marker of a chat replaces its waiting read act and confirms both
/// (Q-L4); a marker with no id is confirmed at once.
#[test]
fn read_acts_page_by_100_and_a_newer_marker_replaces_the_waiting_one() {
    let mut lane = Lane::default();
    assert_eq!(
        lane.push_read(-6, Vec::new(), CmdId(1), None),
        Some(CmdId(1))
    );
    // Chat -7's act runs; two more markers of chat -8 wait, the second replaces the first.
    lane.push_read(-7, vec![1 << 20], CmdId(2), None);
    lane.push_read(-8, (1..=150).map(|n| n << 20).collect(), CmdId(3), None);
    lane.push_read(-8, (1..=250).map(|n| n << 20).collect(), CmdId(4), None);
    let a = lane.next(T0).expect("chat -7");
    assert_eq!(
        lane.on_answer(a, Outcome::Ok, T0),
        read_done(-7, &[2], None)
    );
    let mut sizes = Vec::new();
    loop {
        let page = lane.next(T0).expect("a page of chat -8");
        let LaneAsk::View { chat_id, ids } = &page else {
            panic!("a view page");
        };
        assert_eq!(*chat_id, -8);
        sizes.push(ids.len());
        match lane.on_answer(page, Outcome::Ok, T0) {
            Done::Nothing => {}
            Done::Read {
                cmds,
                chat_id,
                up_to,
            } => {
                assert_eq!((cmds, chat_id, up_to), (vec![CmdId(3), CmdId(4)], -8, None));
                break;
            }
            Done::Fetch(_) => panic!("no fetch"),
        }
    }
    assert_eq!(sizes, [100, 100, 50]);
}

/// The request of a page: `viewMessages` with `force_read: true` (critic H3).
#[test]
fn a_view_page_is_view_messages_with_force_read() {
    let ask = LaneAsk::View {
        chat_id: -9,
        ids: vec![1 << 20, 2 << 20],
    };
    let v = ask.request();
    assert_eq!(v["@type"], "viewMessages");
    assert_eq!(v["chat_id"], -9);
    assert_eq!(v["message_ids"], serde_json::json!([1 << 20, 2 << 20]));
    assert_eq!(v["force_read"], true);
    assert!(v["source"].is_null());
}

/// A restart keeps the read act's page in flight (sent again) and drops the old run's fetches.
#[test]
fn a_restart_keeps_the_read_act_and_drops_the_fetches() {
    let mut lane = Lane::default();
    lane.push_fetch(Fetch::Chat(-1));
    lane.push_read(-2, vec![1 << 20], CmdId(7), None);
    let page = lane.next(T0).expect("the page");
    lane.on_restart();
    assert_eq!(lane.next(T0), Some(page.clone()));
    assert_eq!(
        lane.on_answer(page, Outcome::Ok, T0),
        read_done(-2, &[7], None)
    );
    assert_eq!(
        lane.next(T0 + 10 * FETCH_PACE_MS),
        None,
        "the old run's fetch is gone"
    );
}

/// TF7 (reviews dF7, pF1): a read act's answer decides. A page TDLib REFUSED (an error other than
/// 429) ends the act without confirming it: the command stays pending in the core, which sends it
/// again at the next start. (Mutant: any answer confirms.)
#[test]
fn a_refused_view_page_ends_the_read_act_unconfirmed() {
    let mut lane = Lane::default();
    let ids: Vec<i64> = (1..=150).map(|n| n << 20).collect();
    assert!(lane.push_read(-2, ids, CmdId(8), None).is_none());
    let first = lane.next(T0).expect("the first page");
    assert_eq!(lane.on_answer(first, Outcome::Ok, T0 + 1), Done::Nothing);
    let second = lane.next(T0 + 1).expect("the second page");
    assert_eq!(
        lane.on_answer(second, Outcome::Refused, T0 + 2),
        Done::Nothing,
        "a refused page confirmed the command"
    );
    assert_eq!(lane.next(T0 + 3), None, "the act went on");
    assert_eq!(lane.waiting(), 0);
    // The next marker of the chat is a new act.
    assert!(lane.push_read(-2, vec![1 << 20], CmdId(9), None).is_none());
    let page = lane.next(T0 + 4).expect("a new act");
    assert_eq!(
        lane.on_answer(page, Outcome::Ok, T0 + 5),
        read_done(-2, &[9], None)
    );
}

/// TF7: a page whose answer never came (the link closed, a timeout) is sent again, like a
/// restart does, and the command is confirmed only by TDLib's own `ok`. (Mutant: any result of
/// the last page confirms.)
#[test]
fn a_closed_link_does_not_confirm_a_read_act() {
    let mut lane = Lane::default();
    assert!(lane.push_read(-2, vec![1 << 20], CmdId(7), None).is_none());
    let page = lane.next(T0).expect("the page");
    assert_eq!(
        lane.on_answer(page.clone(), Outcome::Lost, T0 + 1),
        Done::Nothing,
        "a lost answer confirmed the command"
    );
    assert_eq!(lane.next(T0 + 2), Some(page.clone()), "the page goes again");
    assert_eq!(
        lane.on_answer(page, Outcome::Ok, T0 + 3),
        read_done(-2, &[7], None)
    );
}

/// TG-4 K4 (critic M4): a read act's confirm carries its chat and the highest INCOMING id of the
/// act (the session stores it as the read position in the confirm's commit); a newer marker of
/// the same chat that replaces the waiting act keeps the highest one, and an act with no
/// incoming id (only our own messages) adds none. (Mutant: the merge takes the newer marker's
/// value as it is, `None` included.)
#[test]
fn a_read_acts_confirm_carries_its_highest_incoming_id() {
    let mut lane = Lane::default();
    let ids = |n: i64| (1..=n).map(|k| k << 20).collect::<Vec<i64>>();
    assert!(
        lane.push_read(-8, ids(3), CmdId(1), Some(3 << 20))
            .is_none()
    );
    assert!(
        lane.push_read(-8, ids(9), CmdId(2), Some(9 << 20))
            .is_none()
    );
    assert!(lane.push_read(-8, ids(12), CmdId(3), None).is_none());
    let page = lane.next(T0).expect("the one page");
    assert_eq!(
        lane.on_answer(page, Outcome::Ok, T0),
        read_done(-8, &[1, 2, 3], Some(9 << 20)),
        "the merged act's confirm"
    );
    // An act of our own messages alone has none.
    assert!(lane.push_read(-9, vec![1 << 20], CmdId(4), None).is_none());
    let page = lane.next(T0).expect("the page");
    assert_eq!(
        lane.on_answer(page, Outcome::Ok, T0),
        read_done(-9, &[4], None)
    );
}

/// TG-4 K4 (D-2): a read re-assert is `viewMessages(chat, [id], source null, force_read true)`,
/// at most one waiting per chat, goes right after the read act's pages and ahead of every fetch,
/// unpaced, and ends with ANY answer: a refused or a lost one is not retried in the run, no
/// command is confirmed. (Mutant: `Lost` requeues it; the re-assert behind the fetches.)
#[test]
fn a_read_reassert_goes_after_the_read_act_and_is_never_retried() {
    let mut lane = Lane::default();
    lane.push_fetch(Fetch::Chat(-1));
    lane.push_fetch(msg(1));
    lane.push_reassert(-3, 9 << 20);
    lane.push_reassert(-3, 10 << 20);
    lane.push_reassert(-4, 7 << 20);
    assert!(
        lane.push_read(-2, vec![1 << 20], CmdId(1), Some(1 << 20))
            .is_none()
    );
    assert_eq!(lane.waiting(), 5, "two re-asserts, one page, two fetches");
    assert_eq!(lane.due_ms(), Some(0));
    let page = lane.next(T0).expect("the read act first");
    assert!(matches!(page, LaneAsk::View { chat_id: -2, .. }));
    assert_eq!(
        lane.on_answer(page, Outcome::Ok, T0),
        read_done(-2, &[1], Some(1 << 20))
    );
    let first = lane.next(T0).expect("the first re-assert");
    assert_eq!(
        first,
        LaneAsk::Reassert {
            chat_id: -3,
            id: 9 << 20
        },
        "the first one of a chat is kept"
    );
    let v = first.request();
    assert_eq!(v["@type"], "viewMessages");
    assert_eq!(v["chat_id"], -3);
    assert_eq!(v["message_ids"], serde_json::json!([9 << 20]));
    assert_eq!(v["force_read"], true);
    assert!(v["source"].is_null());
    assert_eq!(lane.next(T0), None, "one request in flight");
    assert_eq!(
        lane.on_answer(first, Outcome::Refused, T0 + 1),
        Done::Nothing,
        "a refused re-assert confirmed or retried"
    );
    let second = lane.next(T0 + 1).expect("the second re-assert");
    assert_eq!(
        second,
        LaneAsk::Reassert {
            chat_id: -4,
            id: 7 << 20
        }
    );
    assert_eq!(lane.on_answer(second, Outcome::Lost, T0 + 2), Done::Nothing);
    assert_eq!(
        lane.next(T0 + 2),
        Some(LaneAsk::Fetch(Fetch::Chat(-1))),
        "a lost re-assert was sent again, or went behind a fetch"
    );
}

/// TG-4 K4: a 429 of a re-assert pauses the lane and the same request goes again first (like
/// every request); a helper restart DROPS the waiting and the in-flight re-asserts (the next run
/// computes its own). (Mutant: the restart requeues the one in flight.)
#[test]
fn a_reassert_waits_out_a_429_and_a_restart_drops_it() {
    let mut lane = Lane::default();
    lane.push_reassert(-3, 9 << 20);
    let ask = lane.next(T0).expect("the re-assert");
    assert_eq!(
        lane.on_answer(ask.clone(), Outcome::TooMany(Some(1)), T0),
        Done::Nothing
    );
    assert_eq!(lane.next(T0 + 1_099), None, "sent before the wait was over");
    assert_eq!(lane.next(T0 + 1_100), Some(ask), "the 429's request again");
    lane.push_reassert(-4, 7 << 20);
    lane.on_restart();
    assert_eq!(
        lane.next(T0 + 1_200),
        None,
        "a re-assert survived the restart"
    );
    assert_eq!(lane.waiting(), 0);
    assert_eq!(lane.due_ms(), None);
}

/// TG-4 fix round U1 (durability F1): a LOST answer (the link closed, the request timed out, the
/// frame was not understood) of the chat load or of a fresh-link page is no answer: the ask goes
/// back to the front of the paced fetches (the same request again, paced like any fetch) and the
/// machines hear nothing. A TDLib error is an answer (a refusal keeps what came). Every other
/// fetch keeps handing its lost answer to its machine (the scope of the rule). A restart drops
/// the requeued ask (the next run loads from its own state). (Mutant: the old `Done::Fetch` for a
/// lost start-load answer.)
#[test]
fn a_lost_start_load_answer_goes_again_and_is_never_an_answer() {
    let history = Fetch::History {
        chat_id: -2,
        from: 9 << 20,
        limit: 3,
    };
    for fetch in [Fetch::LoadChats { archive: false }, history] {
        let mut lane = Lane::default();
        lane.push_fetch(fetch);
        let ask = lane.next(T0).expect("the load request");
        assert_eq!(
            lane.on_answer(ask.clone(), Outcome::Lost, T0 + 5),
            Done::Nothing,
            "a lost answer reached the machines: {fetch:?}"
        );
        assert_eq!(lane.in_flight(), None);
        assert_eq!(lane.waiting(), 1, "the ask was not put back");
        assert_eq!(
            lane.due_ms(),
            Some(T0 + FETCH_PACE_MS),
            "paced like any fetch"
        );
        assert_eq!(lane.next(T0 + FETCH_PACE_MS - 1), None);
        assert_eq!(lane.next(T0 + FETCH_PACE_MS), Some(ask.clone()));
        // Lost again: again, with no cap.
        assert_eq!(
            lane.on_answer(ask.clone(), Outcome::Lost, T0 + FETCH_PACE_MS + 5),
            Done::Nothing
        );
        assert_eq!(lane.next(T0 + 2 * FETCH_PACE_MS), Some(ask.clone()));
        // A TDLib error is an answer.
        assert_eq!(
            lane.on_answer(ask.clone(), Outcome::Refused, T0 + 2 * FETCH_PACE_MS + 5),
            Done::Fetch(fetch)
        );
        assert_eq!(lane.waiting(), 0);
        // The helper went away with the ask requeued: the next run asks its own.
        lane.push_fetch(fetch);
        let ask = lane.next(T0 + 10 * FETCH_PACE_MS).expect("the request");
        lane.on_answer(ask, Outcome::Lost, T0 + 10 * FETCH_PACE_MS + 1);
        assert_eq!(lane.waiting(), 1);
        lane.on_restart();
        assert_eq!(lane.waiting(), 0, "a restart kept the lost start load");
        assert_eq!(lane.next(T0 + 20 * FETCH_PACE_MS), None);
    }
    // The scope: the reconcile's page and a message lookup keep the old rule.
    let gap = Fetch::Gap {
        chat_id: -2,
        above: 9 << 20,
        from: 9 << 20,
        limit: 50,
    };
    for fetch in [gap, msg(1)] {
        let mut lane = Lane::default();
        lane.push_fetch(fetch);
        let ask = lane.next(T0).expect("the request");
        assert_eq!(
            lane.on_answer(ask, Outcome::Lost, T0 + 5),
            Done::Fetch(fetch),
            "the rule reached another fetch: {fetch:?}"
        );
    }
}
