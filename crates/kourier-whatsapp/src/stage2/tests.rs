//! Stage 2 (W3a, chunk C3): rows -> events in one zone. The hook-level kill loops (a child killed at
//! the failpoints) are chunk C4; the actor test (the stage runs at start and its events ship) is in
//! `actor/tests.rs`.
#![allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, FakeClock, IncomingMessage, decode_outbox_row};
use kourier_db::{DbKey, outbox};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use waproto::whatsapp as wa;

use super::{MAX_ATTEMPTS, MAX_ROWS, Stage2, Timing};
use crate::error::sql;
use crate::fault;
use crate::hook::Notes;
use crate::inbox::{Arrival, Inbox, InboxMeta, Origin};
use crate::store::{KourierStore, StoreOwner};

const START: u64 = 1_700_000_200_000;
const PEER: &str = "15550100101@s.whatsapp.net";

struct World {
    _dir: tempfile::TempDir,
    owner: StoreOwner,
    inbox: Inbox,
    stage2: Arc<Stage2>,
    clock: FakeClock,
    outbox_wake: Arc<Notify>,
}

fn world(timing: Timing) -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([6; 32]),
        "wa-stage2",
    )
    .expect("open");
    let clock = FakeClock::new(START);
    let outbox_wake = Arc::new(Notify::new());
    let inbox = Inbox::new(owner.store(), Arc::new(clock.clone()));
    let stage2 = Arc::new(Stage2::new(
        owner.store(),
        Arc::new(clock.clone()),
        outbox_wake.clone(),
        timing,
    ));
    World {
        _dir: dir,
        owner,
        inbox,
        stage2,
        clock,
        outbox_wake,
    }
}

fn quick() -> Timing {
    Timing {
        db_failure_pause: Duration::from_millis(20),
        ..Timing::default()
    }
}

fn text(body: &str) -> wa::Message {
    let mut message = wa::Message::default();
    message.conversation = Some(body.to_owned());
    message
}

fn arrival_in(chat: &str, id: &str, body: &str, stamp: i64, timer: Option<u32>) -> Arrival {
    Arrival {
        chat: chat.into(),
        sender: chat.into(),
        id: id.into(),
        origin: Origin::E2e,
        message: waproto::codec::message_to_vec(&text(body)),
        meta: InboxMeta {
            v: InboxMeta::VERSION,
            timestamp: stamp,
            from_me: false,
            is_group: false,
            push_name: String::new(),
            lid_addressing: false,
            sender_alt: None,
            recipient_alt: None,
            ephemeral_expiration: timer,
            is_offline: false,
        },
    }
}

fn arrival(id: &str, body: &str) -> Arrival {
    arrival_in(PEER, id, body, 1_700_000_000, None)
}

async fn commit(w: &World, batch: Vec<Arrival>) {
    let done = w.inbox.commit(batch, Vec::new()).await.expect("commit");
    w.stage2.committed(&done);
}

/// Every message event in `core_outbox`, in order.
async fn messages(store: &KourierStore) -> Vec<IncomingMessage> {
    store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                for event in decode_outbox_row(&row.envelope).expect("decodes") {
                    if let BackendEvent::Message(m) = event {
                        out.push(m);
                    }
                }
            }
            Ok(out)
        })
        .await
        .expect("read the outbox")
}

/// The names of every event in `core_outbox`, in order.
async fn event_names(store: &KourierStore) -> Vec<&'static str> {
    store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                for event in decode_outbox_row(&row.envelope).expect("decodes") {
                    // The chats' features ride with their first rows (D9): a test of their own reads them.
                    if !crate::features::is_statement(&event) {
                        out.push(event.name());
                    }
                }
            }
            Ok(out)
        })
        .await
        .expect("read the outbox")
}

async fn count(store: &KourierStore, sql_text: &'static str) -> i64 {
    store
        .read(move |conn| {
            conn.query_row(sql_text, [], |r| r.get(0))
                .map_err(sql("count rows"))
        })
        .await
        .expect("count")
}

async fn states(store: &KourierStore) -> (i64, i64) {
    (
        count(store, "SELECT count(*) FROM inbox WHERE state = 'new'").await,
        count(store, "SELECT count(*) FROM inbox WHERE state = 'done'").await,
    )
}

#[tokio::test]
async fn a_replayed_message_becomes_one_event() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let batch = || {
        vec![
            arrival("A", "one"),
            arrival("B", "two"),
            arrival("C", "three"),
        ]
    };
    commit(&w, batch()).await;
    let first = w.stage2.step().await.expect("step");
    assert_eq!((first.taken, first.finished, first.events), (3, 3, 3));
    let events = messages(&store).await;
    let locals: Vec<&str> = events.iter().map(|m| &*m.local).collect();
    assert_eq!(
        locals,
        ["A", "B", "C"],
        "one event per message, in arrival order"
    );
    let arrivals: Vec<u64> = events.iter().filter_map(|m| m.arrival).collect();
    assert!(arrivals.windows(2).all(|w| w[0] < w[1]));

    // Upstream sends the batch again (the ack was lost): the ledger absorbs it, even after stage 2.
    let replay = w.inbox.commit(batch(), Vec::new()).await.expect("replay");
    assert_eq!(replay.inserted, 0);
    let again = w.stage2.step().await.expect("step");
    assert_eq!(again.taken, 0, "nothing new to take");
    assert_eq!(
        messages(&store).await.len(),
        3,
        "still one event per message"
    );
    assert_eq!(states(&store).await, (0, 3));
    w.owner.close();
}

#[tokio::test]
async fn a_finished_row_keeps_the_ledger_and_loses_its_content() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(&w, vec![arrival("A", "secret words")]).await;
    w.stage2.step().await.expect("step");
    let (wiped, ledger): (i64, i64) = store
        .read(|conn| {
            conn.query_row(
                "SELECT count(*) FILTER (WHERE message IS NULL AND meta IS NULL AND state = 'done'), \
                        count(*) FILTER (WHERE chat != '' AND sender != '' AND id = 'A') FROM inbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql("read the ledger"))
        })
        .await
        .expect("ledger");
    assert_eq!((wiped, ledger), (1, 1));
    w.owner.close();
}

#[tokio::test]
async fn a_step_is_one_zone_a_failed_commit_leaves_the_rows_new_and_no_event() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(
        &w,
        vec![
            arrival("A", "one"),
            arrival_in(PEER, "B", "two", 1_700_000_001, Some(3600)),
        ],
    )
    .await;
    fault::arm(fault::STAGE2_COMMIT, 1);
    assert!(
        w.stage2.step().await.is_err(),
        "the armed seam fails the zone"
    );
    assert_eq!(states(&store).await, (2, 0), "every row is still new");
    assert_eq!(
        messages(&store).await.len(),
        0,
        "no event of a rolled-back zone"
    );
    // The next step does all of it, once.
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.finished, report.events), (2, 2));
    assert_eq!(states(&store).await, (0, 2));
    assert_eq!(messages(&store).await.len(), 2);
    w.owner.close();
}

#[tokio::test]
async fn a_step_takes_at_most_fifty_rows_in_arrival_order() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let batch: Vec<Arrival> = (0..120)
        .map(|n| arrival(&format!("M{n:03}"), "x"))
        .collect();
    commit(&w, batch).await;
    assert_eq!(w.stage2.backlog(), 120);
    let first = w.stage2.step().await.expect("step");
    assert_eq!((first.taken, first.remaining), (MAX_ROWS, 70));
    assert_eq!(w.stage2.backlog(), 70, "the step publishes what is left");
    let events = messages(&store).await;
    assert_eq!(&*events[0].local, "M000");
    assert_eq!(&*events[MAX_ROWS - 1].local, "M049");
    while w.stage2.step().await.expect("step").taken > 0 {}
    let all = messages(&store).await;
    assert_eq!(all.len(), 120);
    assert!(all.windows(2).all(|w| w[0].arrival < w[1].arrival));
    assert_eq!(w.stage2.backlog(), 0);
    w.owner.close();
}

#[tokio::test]
async fn a_failing_row_is_parked_then_finished_with_a_placeholder_and_never_holds_the_rest_back() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let mut bad = arrival("BAD", "x");
    bad.message = vec![0xff; 6];
    commit(&w, vec![arrival("A", "one"), bad, arrival("C", "three")]).await;
    let first = w.stage2.step().await.expect("step");
    assert_eq!((first.finished, first.parked, first.poisoned), (2, 1, 0));
    assert_eq!(
        states(&store).await,
        (1, 2),
        "the bad row waits, its neighbours are done"
    );
    // Not due yet: nothing to take.
    assert_eq!(w.stage2.step().await.expect("step").taken, 0);
    assert_eq!(w.stage2.backlog(), 0, "a parked row is not backlog");
    // Each retry is due after the longest park; the tenth failure finishes it.
    let mut last = first;
    for n in 1..MAX_ATTEMPTS {
        w.clock.set(START + u64::from(n) * 700_000);
        last = w.stage2.step().await.expect("step");
        assert_eq!(last.taken, 1, "attempt {}", n + 1);
    }
    assert_eq!((last.parked, last.poisoned, last.events), (0, 1, 1));
    assert_eq!(states(&store).await, (0, 3));
    let events = messages(&store).await;
    assert_eq!(events.len(), 3);
    assert_eq!(&*events[2].local, "BAD");
    assert!(matches!(
        events[2].body,
        kourier_model::Body::Placeholder(_)
    ));
    w.owner.close();
}

#[tokio::test]
async fn a_message_without_a_timer_has_no_expiry_whatever_the_chat_showed_before() {
    // Fix F1: a chat taught a day-long timer by one message; the peer switches it off; the next
    // messages carry none. Nothing remembers the old timer, so nothing erases them.
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(
        &w,
        vec![arrival_in(PEER, "A", "one", 1_700_000_000, Some(86_400))],
    )
    .await;
    w.stage2.step().await.expect("step");
    commit(&w, vec![arrival_in(PEER, "B", "two", 1_700_000_100, None)]).await;
    w.stage2.step().await.expect("step");
    let events = messages(&store).await;
    assert_eq!(
        events[0].expiry.expect("A carried it").timer.as_secs(),
        86_400
    );
    assert!(events[1].expiry.is_none(), "B carried none: no expiry");
    w.owner.close();
}

fn arrival_of(chat: &str, sender: &str, id: &str, message: &wa::Message) -> Arrival {
    Arrival {
        sender: sender.into(),
        message: waproto::codec::message_to_vec(message),
        ..arrival_in(chat, id, "", 1_700_000_000, None)
    }
}

async fn deferred(store: &KourierStore) -> (i64, i64) {
    (
        count(store, "SELECT count(*) FROM inbox WHERE state = 'deferred'").await,
        count(
            store,
            "SELECT count(*) FROM inbox WHERE state = 'deferred' AND message IS NOT NULL AND meta IS NOT NULL",
        )
        .await,
    )
}

#[tokio::test]
async fn a_sender_of_a_kind_this_build_does_not_map_is_deferred_with_its_content_not_poisoned() {
    // Fix F3: a message from a @bot sender cannot be mapped by this build, and no retry will
    // change that. It stays, with its content, past the ten tries the poison rule allows.
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(
        &w,
        vec![
            arrival("A", "one"),
            arrival_of(PEER, "123@bot", "H", &text("from a bot sender")),
        ],
    )
    .await;
    let first = w.stage2.step().await.expect("step");
    assert_eq!(
        (first.finished, first.deferred, first.parked, first.poisoned),
        (1, 1, 0, 0)
    );
    assert_eq!(states(&store).await, (0, 1));
    assert_eq!(deferred(&store).await, (1, 1), "deferred, content kept");
    assert_eq!(w.stage2.backlog(), 0, "a deferred row is no backlog");
    // Long past the poison rule's tries (and its 45 days) the row is still there, untouched and
    // not looked at again.
    for n in 1..=u64::from(MAX_ATTEMPTS) + 2 {
        w.clock.set(START + n * 4_000_000_000);
        assert_eq!(w.stage2.step().await.expect("step").taken, 0, "round {n}");
    }
    assert_eq!(deferred(&store).await, (1, 1));
    assert_eq!(
        count(&store, "SELECT count(*) FROM inbox WHERE state = 'done'").await,
        1
    );
    assert_eq!(messages(&store).await.len(), 1, "only A has an event");
    w.owner.close();
}

#[tokio::test]
async fn a_poll_vote_is_deferred_with_its_content_and_a_key_share_is_wiped() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    // A poll vote is a kind no step before K8a maps: kept with its content.
    let mut vote_message = wa::Message::default();
    vote_message.poll_update_message =
        waproto::buffa::MessageField::some(wa::message::PollUpdateMessage::default());
    let mut share = wa::message::ProtocolMessage::default();
    share.app_state_sync_key_share =
        waproto::buffa::MessageField::some(wa::message::AppStateSyncKeyShare::default());
    let mut share_message = wa::Message::default();
    share_message.protocol_message = waproto::buffa::MessageField::some(share);
    commit(
        &w,
        vec![
            arrival_of(PEER, PEER, "VOTE", &vote_message),
            arrival_of(PEER, PEER, "KEYS", &share_message),
        ],
    )
    .await;
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.finished, report.deferred, report.events), (1, 1, 0));
    assert_eq!(
        deferred(&store).await,
        (1, 1),
        "the poll vote keeps its content"
    );
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM inbox WHERE state = 'done' AND id = 'KEYS' AND message IS NULL"
        )
        .await,
        1,
        "the key share is the one thing that is wiped"
    );
    w.owner.close();
}

/// W4 (critic H1): the notice of a chunk our phone is sending is recorded in the zone that finishes its
/// row. A zone that fails leaves the row new and NOTHING recorded (a kill cannot leave a finished
/// notice nobody counted); the next step does both; a peer's notice is finished and records nothing;
/// the same id announced twice is one record.
#[tokio::test]
async fn an_announced_chunk_is_recorded_in_the_zone_that_finishes_its_row() {
    use wa::message::protocol_message::Type;
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let notice = |fill: &dyn Fn(&mut wa::message::ProtocolMessage)| {
        let mut inner = wa::message::ProtocolMessage::default();
        fill(&mut inner);
        let mut message = wa::Message::default();
        message.protocol_message = waproto::buffa::MessageField::some(inner);
        message
    };
    let by_type = notice(&|p| p.r#type = Some(Type::HISTORY_SYNC_NOTIFICATION));
    let mut ours = arrival_of(PEER, PEER, "CHUNK1", &by_type);
    ours.meta.from_me = true;
    let theirs = arrival_of(PEER, PEER, "SPOOF", &by_type);
    commit(&w, vec![ours, theirs]).await;
    fault::arm(fault::STAGE2_COMMIT, 1);
    assert!(
        w.stage2.step().await.is_err(),
        "the armed seam fails the zone"
    );
    assert_eq!(states(&store).await, (2, 0), "both rows are still new");
    assert_eq!(
        count(&store, "SELECT count(*) FROM history_announced").await,
        0,
        "a rolled-back zone records nothing"
    );
    let report = w.stage2.step().await.expect("step");
    assert_eq!(
        (report.finished, report.deferred, report.events),
        (2, 0, 0),
        "both are finished, neither is kept, neither gives an event"
    );
    assert_eq!(states(&store).await, (0, 2));
    assert_eq!(
        count(&store, "SELECT count(*) FROM history_announced").await,
        1,
        "only our phone's notice is recorded"
    );
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM history_announced \
             WHERE msg_id = 'CHUNK1' AND announced_at = 1700000200000 AND lost = 0"
        )
        .await,
        1,
        "the stanza id, the wall clock, not lost"
    );
    assert_eq!(deferred(&store).await, (0, 0), "nothing is kept");
    // The same notice taken up again (a newer map after a deferral, a second row of one id) records
    // nothing new.
    let mut again = arrival_of(PEER, PEER, "CHUNK1", &by_type);
    again.meta.from_me = true;
    again.chat = "15550100102@s.whatsapp.net".into();
    again.sender = again.chat.clone();
    commit(&w, vec![again]).await;
    w.stage2.step().await.expect("step");
    assert_eq!(
        count(&store, "SELECT count(*) FROM history_announced").await,
        1,
        "one id is one record"
    );
    w.owner.close();
}

#[tokio::test]
async fn a_later_maps_version_takes_up_what_an_older_one_deferred() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(
        &w,
        vec![
            arrival_of(PEER, "123@bot", "H", &text("still unmapped")),
            arrival_of(PEER, "123@bot", "OLD", &text("deferred long ago")),
        ],
    )
    .await;
    w.stage2.step().await.expect("step");
    assert_eq!(deferred(&store).await, (2, 2));
    assert_eq!(
        count(&store, "SELECT count(*) FROM inbox WHERE map_version >= 1").await,
        2,
        "the version of the map that deferred them is kept"
    );
    // The current map deferred both: starting again takes nothing up.
    assert_eq!(w.stage2.take_up_deferred().await.expect("take up"), 0);
    // An earlier map deferred one of them: the current one tries it again.
    store
        .write("age a row", |tx| {
            tx.execute("UPDATE inbox SET map_version = 0 WHERE id = 'OLD'", [])
                .map_err(sql("age a row"))
                .map(drop)
        })
        .await
        .expect("age");
    assert_eq!(w.stage2.take_up_deferred().await.expect("take up"), 1);
    assert_eq!(states(&store).await, (1, 0));
    let report = w.stage2.step().await.expect("step");
    assert_eq!(
        (report.taken, report.deferred),
        (1, 1),
        "tried, still unmapped"
    );
    assert_eq!(deferred(&store).await, (2, 2));
    w.owner.close();
}

/// Fix F12 (the lead's Q8.8): the poison rule's age counts from the arrival or from the last take-up,
/// whichever is later. A row that waited more than 45 days for a map that can try it fails for content
/// at its first try with a cause that may be fixed by the next build; it is parked like any failing row,
/// not ended with a placeholder at once. The age then runs from the take-up.
#[tokio::test]
async fn a_row_taken_up_after_a_long_wait_is_not_poisoned_at_its_first_failure() {
    let _serial = fault::serial();
    const DAY_MS: u64 = 24 * 3_600 * 1_000;
    let w = world(quick());
    let store = w.owner.store();
    let mut bad = arrival("OLDBAD", "x");
    bad.message = vec![0xff; 6];
    commit(&w, vec![bad]).await;
    // An earlier map deferred it (state 'deferred'), and it waited 50 days.
    store
        .write("plant a deferral", |tx| {
            tx.execute(
                "UPDATE inbox SET state = 'deferred', map_version = 0 WHERE id = 'OLDBAD'",
                [],
            )
            .map_err(sql("plant"))
            .map(drop)
        })
        .await
        .expect("plant");
    w.clock.set(START + 50 * DAY_MS);
    assert_eq!(w.stage2.take_up_deferred().await.expect("take up"), 1);
    let first = w.stage2.step().await.expect("step");
    assert_eq!(
        (first.taken, first.parked, first.poisoned),
        (1, 1, 0),
        "tried and parked, not ended for being 50 days old"
    );
    assert_eq!(states(&store).await, (1, 0));
    assert!(messages(&store).await.is_empty(), "no placeholder yet");
    // The row's own arrival time is untouched; the age base is the take-up.
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM inbox WHERE id = 'OLDBAD' AND received_at = 1700000200000 \
             AND aged_from = 1700000200000 + 50 * 86400000"
        )
        .await,
        1
    );
    // 46 days after the take-up it is past the rule's age, from the take-up: the next failure ends it.
    w.clock.set(START + (50 + 46) * DAY_MS);
    let last = w.stage2.step().await.expect("step");
    assert_eq!(
        (last.taken, last.poisoned),
        (1, 1),
        "old again, from the take-up"
    );
    assert_eq!(states(&store).await, (0, 1));
    w.owner.close();
}

/// W3b: one inbox row can stand for several events (a merge of two ids, the chat's upsert, the
/// message): they are ONE `core_outbox` row, in the order the map gave, so no kill can split them.
#[tokio::test]
async fn one_row_of_several_events_is_one_outbox_row_in_order() {
    use kourier_backend_api::IncomingMessage;
    use kourier_model::conversation::ConversationPatch;
    use kourier_model::{Body, ConvKey, Origin as Sender, UserId};

    use crate::map::{Mapped, Row};

    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let pn = UserId::from_parts("pn", "15550100101").expect("pn");
    let lid = UserId::from_parts("lid", "99900100101").expect("lid");
    let conv = ConvKey::Direct(lid.clone());
    let message = IncomingMessage::new(
        conv.clone(),
        "3EB0AA01",
        Sender::Remote {
            author: lid.clone(),
            device: None,
        },
        Body::text("one of three"),
        1_700_000_000_000,
        1_700_000_100_000,
    );
    let events = vec![
        BackendEvent::IdentityMerged {
            from: pn,
            into: lid,
        },
        BackendEvent::ConversationUpsert(ConversationPatch::new(conv)),
        BackendEvent::Message(message),
    ];
    let row = Row {
        arrival: 1,
        chat: PEER.into(),
        sender: PEER.into(),
        id: "3EB0AA01".into(),
        origin: Origin::E2e,
        received_at: 1_700_000_100_000,
        catch_up: false,
        message: Vec::new(),
        meta: Vec::new(),
        attempts: 0,
        aged_from: 0,
    };
    let plan = vec![super::Planned::new(
        row,
        Ok(Mapped {
            events,
            ..Mapped::inert()
        }),
        1_700_000_200_000,
    )];
    let report = store
        .write_at("test zone", super::POINTS, move |tx| {
            super::apply(tx, 1_700_000_200_000, &plan)
        })
        .await
        .expect("the zone");
    assert_eq!((report.events, report.finished), (1, 1), "one row");
    let rows = store
        .read(|conn| {
            let rows = outbox::peek(conn, 100).map_err(|_| crate::error::database("peek"))?;
            Ok(rows
                .into_iter()
                .map(|r| decode_outbox_row(&r.envelope).expect("decodes"))
                .collect::<Vec<_>>())
        })
        .await
        .expect("read");
    assert_eq!(rows.len(), 1, "ONE core_outbox row for the three events");
    // The chat's features (W5a, D9) come after the merge, before the chat's first event: their test is
    // `a_chat_is_stated_once_in_the_row_of_its_first_message`.
    let kinds: Vec<&str> = rows[0]
        .iter()
        .filter(|e| !crate::features::is_statement(e))
        .map(|e| match e {
            BackendEvent::IdentityMerged { .. } => "merge",
            BackendEvent::ConversationUpsert(_) => "upsert",
            BackendEvent::Message(_) => "message",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["merge", "upsert", "message"], "in the map's order");
    w.owner.close();
}

/// D1, the critic's H3 and the rule of `MAP_VERSION`: the rows W3a deferred (it deferred every protocol
/// message and reaction, and everything under its one `Skip`) are taken up again by the next start. A
/// revoke, an edit and a reaction give their events; a status is let go (finished and wiped), a
/// broadcast-list message from somebody else is mapped into the direct chat with its sender, and every
/// kind W3b still defers is deferred again with the new version and its content.
#[tokio::test]
async fn every_kind_w3a_deferred_now_maps_or_stays_deferred_by_rule() {
    use wa::message::protocol_message::Type;
    use waproto::buffa::MessageField;
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let mut vote = wa::Message::default();
    vote.poll_update_message = MessageField::some(wa::message::PollUpdateMessage::default());
    let mut enc_reaction = wa::Message::default();
    enc_reaction.enc_reaction_message =
        MessageField::some(wa::message::EncReactionMessage::default());
    let mut album = wa::Message::default();
    album.album_message = MessageField::some(wa::message::AlbumMessage::default());
    let key = |id: &str| {
        let mut key = wa::MessageKey::default();
        key.id = Some(id.into());
        key.from_me = Some(true);
        MessageField::some(key)
    };
    let mut revoke = wa::message::ProtocolMessage::default();
    revoke.r#type = Some(Type::REVOKE);
    revoke.key = key("TARGET1");
    let mut revoke_message = wa::Message::default();
    revoke_message.protocol_message = MessageField::some(revoke);
    let mut edit = wa::message::ProtocolMessage::default();
    edit.r#type = Some(Type::MESSAGE_EDIT);
    edit.key = key("TARGET2");
    edit.edited_message = MessageField::some(text("the new text"));
    let mut edit_message = wa::Message::default();
    edit_message.protocol_message = MessageField::some(edit);
    let mut reaction = wa::message::ReactionMessage::default();
    reaction.key = key("TARGET3");
    reaction.text = Some("👍".into());
    let mut reaction_message = wa::Message::default();
    reaction_message.reaction_message = MessageField::some(reaction);
    let mut own_list = arrival_of(
        "1700000000@broadcast",
        "99900100123:2@lid",
        "OWNLIST",
        &text("my message to a list"),
    );
    own_list.meta.from_me = true;
    let mut channel = arrival("CHANNEL", "channel post");
    channel.origin = Origin::Newsletter;
    commit(
        &w,
        vec![
            arrival_of(PEER, PEER, "REVOKE", &revoke_message),
            arrival_of(PEER, PEER, "EDIT", &edit_message),
            arrival_of(PEER, PEER, "REACTION", &reaction_message),
            arrival_of("status@broadcast", PEER, "STATUS", &text("a status")),
            arrival_of("1700000000@broadcast", PEER, "LIST", &text("for a list")),
            own_list,
            arrival_of(PEER, PEER, "VOTE", &vote),
            arrival_of(PEER, PEER, "ENCREACT", &enc_reaction),
            arrival_of(PEER, PEER, "ALBUM", &album),
            arrival_of(PEER, "123@bot", "BOT", &text("bot sender")),
            channel,
        ],
    )
    .await;
    // The state W3a left them in: all eleven deferred by the map of version 1.
    store
        .write("plant a W3a state", |tx| {
            tx.execute("UPDATE inbox SET state = 'deferred', map_version = 1", [])
                .map_err(sql("plant"))
                .map(drop)
        })
        .await
        .expect("plant");
    assert_eq!(deferred(&store).await, (11, 11));
    // The next start takes them up, the step maps them.
    assert_eq!(w.stage2.take_up_deferred().await.expect("take up"), 11);
    let report = w.stage2.step().await.expect("step");
    assert_eq!(report.taken, 11);
    // REVOKE, EDIT and REACTION give their events; STATUS is let go; LIST is a message. Six stay
    // deferred: the own list message, the vote, the encrypted reaction, the album parent, the bot
    // sender and the channel post.
    assert_eq!((report.finished, report.deferred, report.events), (5, 6, 4));
    assert_eq!(
        event_names(&store).await,
        ["Deleted", "Edited", "Reaction", "Message"],
        "one event each, in arrival order"
    );
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM inbox WHERE state = 'done' AND message IS NULL \
             AND id IN ('REVOKE', 'EDIT', 'REACTION', 'STATUS', 'LIST')"
        )
        .await,
        5,
        "the five are finished and wiped"
    );
    let shown = messages(&store).await;
    assert_eq!(shown.len(), 1, "only the list message is a message");
    assert_eq!(shown[0].conv.encode(), "direct:pn:15550100101");
    assert_eq!(&*shown[0].local, "LIST");
    assert_eq!(
        deferred(&store).await,
        (6, 6),
        "six kept with their content"
    );
    assert_eq!(
        count(
            &store,
            // The current `MAP_VERSION` (4 since W6a2; `map::tests` pins it).
            "SELECT count(*) FROM inbox WHERE state = 'deferred' AND map_version = 4"
        )
        .await,
        6,
        "kept by the new version"
    );
    // A second start of the same build takes nothing up.
    assert_eq!(w.stage2.take_up_deferred().await.expect("take up"), 0);
    w.owner.close();
}

#[tokio::test]
async fn a_newsletter_item_is_kept_without_an_event_until_channels_are_shown() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let mut post = arrival("N", "channel post");
    post.origin = Origin::Newsletter;
    commit(&w, vec![post]).await;
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.finished, report.deferred, report.events), (0, 1, 0));
    assert_eq!(deferred(&store).await, (1, 1), "kept with its content");
    assert_eq!(messages(&store).await.len(), 0);
    w.owner.close();
}

const DAY_MS: i64 = 24 * 3_600 * 1_000;

/// Sets the arrival time of the inbox rows named by id to `days` before the fake clock's start.
async fn arrived_days_ago(store: &KourierStore, rows: &[(&'static str, i64)]) {
    let rows: Vec<(&'static str, i64)> = rows.to_vec();
    store
        .write("age rows", move |tx| {
            for (id, days) in rows {
                let at = i64::try_from(START).expect("time") - days * DAY_MS;
                tx.execute("UPDATE inbox SET received_at = ?2 WHERE id = ?1", (id, at))
                    .map_err(sql("age"))?;
            }
            Ok(())
        })
        .await
        .expect("age");
}

/// W6a1 (critic L4): a deferred NEWSLETTER item that arrived more than 14 days ago is finished with no event and its
/// content wiped; a younger one keeps its content, and so does an older item of another kind (a deferred end-to-end row
/// waits for its map up to the poison rule's 45 days). Mutant: the bound on every deferred kind.
#[tokio::test]
async fn an_old_newsletter_item_is_finished_and_a_younger_one_or_another_kind_stays() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let mut old = arrival("NOLD", "an old post");
    old.origin = Origin::Newsletter;
    let mut young = arrival("NYOUNG", "a young post");
    young.origin = Origin::Newsletter;
    commit(
        &w,
        vec![
            old,
            young,
            arrival_of(PEER, "123@bot", "EOLD", &text("unmapped for long")),
        ],
    )
    .await;
    w.stage2.step().await.expect("step");
    assert_eq!(deferred(&store).await, (3, 3));
    arrived_days_ago(&store, &[("NOLD", 15), ("NYOUNG", 13), ("EOLD", 20)]).await;

    assert_eq!(w.stage2.finish_old_newsletters().await.expect("finish"), 1);
    assert_eq!(
        deferred(&store).await,
        (2, 2),
        "the younger post and the other kind keep their content"
    );
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM inbox WHERE id = 'NOLD' AND state = 'done' \
             AND message IS NULL AND meta IS NULL"
        )
        .await,
        1,
        "the old post is done and its content wiped"
    );
    assert!(
        event_names(&store).await.is_empty(),
        "no event for a post nobody shows"
    );
    assert_eq!(
        w.stage2.finish_old_newsletters().await.expect("again"),
        0,
        "finished once"
    );
    w.owner.close();
}

/// W6a1 (critic L4): the bound counts from the item's ARRIVAL. A map-version bump takes the item up again and restarts
/// the poison rule's age (`aged_from`), not this one. Mutant: the bound by `aged_from`.
#[tokio::test]
async fn a_map_version_bump_does_not_restart_a_newsletter_items_clock() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let mut post = arrival("NOLD", "an old post");
    post.origin = Origin::Newsletter;
    commit(&w, vec![post]).await;
    w.stage2.step().await.expect("step");
    // It arrived 15 days ago and an EARLIER map deferred it.
    arrived_days_ago(&store, &[("NOLD", 15)]).await;
    store
        .write("an earlier map", |tx| {
            tx.execute("UPDATE inbox SET map_version = 0", [])
                .map_err(sql("earlier"))
                .map(drop)
        })
        .await
        .expect("earlier");
    // This build takes it up (the poison rule's age starts now), tries it and defers it again.
    assert_eq!(w.stage2.take_up_deferred().await.expect("take up"), 1);
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.taken, report.deferred), (1, 1));
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM inbox WHERE id = 'NOLD' AND aged_from = 1700000200000"
        )
        .await,
        1,
        "the take-up restarted the poison rule's age"
    );
    assert_eq!(
        w.stage2.finish_old_newsletters().await.expect("finish"),
        1,
        "the post is still 15 days old by its arrival"
    );
    w.owner.close();
}

/// W6a1 (critic L4): the bound runs at the task's start, before the deferred rows are taken up. Mutant: the task does
/// not call it.
#[tokio::test]
async fn the_task_finishes_old_newsletter_items_at_its_start() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let mut post = arrival("NOLD", "an old post");
    post.origin = Origin::Newsletter;
    commit(&w, vec![post]).await;
    w.stage2.step().await.expect("step");
    arrived_days_ago(&store, &[("NOLD", 15)]).await;
    let stop = CancellationToken::new();
    let task = tokio::spawn(w.stage2.clone().drive(stop.clone()));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while count(&store, "SELECT count(*) FROM inbox WHERE state = 'done'").await != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old post was not finished at the start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("it stops")
        .expect("no panic");
    w.owner.close();
}

#[tokio::test]
async fn the_task_handles_rows_left_at_start_and_wakes_after_a_commit_and_stops() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    // Rows a previous run left.
    commit(&w, vec![arrival("A", "one")]).await;
    let mut progress = w.stage2.progress();
    let stop = CancellationToken::new();
    let task = tokio::spawn(w.stage2.clone().drive(stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), progress.changed())
        .await
        .expect("the first step")
        .expect("open");
    assert_eq!(
        states(&store).await,
        (0, 1),
        "the leftover row was handled at start"
    );
    tokio::time::timeout(Duration::from_secs(10), w.outbox_wake.notified())
        .await
        .expect("the forwarder was woken for the new event");
    // A later commit wakes it.
    commit(&w, vec![arrival("B", "two")]).await;
    tokio::time::timeout(Duration::from_secs(10), progress.changed())
        .await
        .expect("the second step")
        .expect("open");
    assert_eq!(states(&store).await, (0, 2));
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("it stops")
        .expect("no panic");
    w.owner.close();
}

#[tokio::test]
async fn the_task_retries_after_a_failed_step_and_the_rows_are_not_lost() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(&w, vec![arrival("A", "one")]).await;
    fault::arm(fault::STAGE2_COMMIT, 1);
    let mut progress = w.stage2.progress();
    let stop = CancellationToken::new();
    let task = tokio::spawn(w.stage2.clone().drive(stop.clone()));
    // The first step fails and the clock never moves: the pause is the fake clock's, so move it.
    for _ in 0..200 {
        if tokio::time::timeout(Duration::from_millis(50), progress.changed())
            .await
            .is_ok()
        {
            break;
        }
        w.clock.advance(Duration::from_millis(100));
    }
    assert_eq!(
        states(&store).await,
        (0, 1),
        "the retry finished the row once"
    );
    assert_eq!(messages(&store).await.len(), 1);
    stop.cancel();
    task.await.expect("no panic");
    w.owner.close();
}

#[tokio::test]
async fn a_full_backlog_holds_the_hook_until_stage_2_has_room_and_not_for_ever() {
    let _serial = fault::serial();
    let w = world(Timing {
        max_backlog: 2,
        room_patience: Duration::from_secs(60),
        ..quick()
    });
    commit(&w, vec![arrival("A", "one"), arrival("B", "two")]).await;
    assert_eq!(w.stage2.backlog(), 2);
    let stage2 = w.stage2.clone();
    let mut room = Box::pin(async move { stage2.room().await });
    assert!(
        futures::poll!(room.as_mut()).is_pending(),
        "a full backlog holds the hook"
    );
    w.stage2.step().await.expect("step");
    assert!(
        futures::poll!(room.as_mut()).is_ready(),
        "room after the step"
    );

    // A stage 2 that cannot run does not hang the connection: the wait ends after its patience.
    commit(&w, vec![arrival("C", "three"), arrival("D", "four")]).await;
    let stage2 = w.stage2.clone();
    let mut room = Box::pin(async move { stage2.room().await });
    assert!(futures::poll!(room.as_mut()).is_pending());
    w.clock.advance(Duration::from_secs(61));
    assert!(futures::poll!(room.as_mut()).is_ready(), "patience ran out");
    w.owner.close();
}

#[tokio::test]
async fn an_empty_inbox_is_idle_and_changes_nothing() {
    let _serial = fault::serial();
    let w = world(quick());
    let report = w.stage2.step().await.expect("step");
    assert_eq!(report, super::Report::default());
    assert_eq!(
        count(&w.owner.store(), "SELECT count(*) FROM core_outbox").await,
        0
    );
    w.owner.close();
}

/// Fix F5: a step does nothing while the link hold is closed (rows stay `new`, no event reaches
/// `core_outbox`), and takes the rows up once it is open.
#[tokio::test]
async fn a_step_leaves_the_rows_alone_while_a_link_has_no_account() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    store.hold().set_closed(true);
    commit(&w, vec![arrival("A", "one"), arrival("B", "two")]).await;
    let held = w.stage2.step().await.expect("step");
    assert_eq!((held.taken, held.events), (0, 0));
    assert_eq!(states(&store).await, (2, 0), "the rows stay new");
    assert!(messages(&store).await.is_empty());
    store.hold().set_closed(false);
    let open = w.stage2.step().await.expect("step");
    assert_eq!((open.taken, open.events), (2, 2));
    assert_eq!(states(&store).await, (0, 2));
    w.owner.close();
}

/// A chat of the history owes its end: a push can still state a hint, so `history::live::wanted` is true.
async fn open_a_push(store: &KourierStore) {
    store
        .write("test push", |tx| {
            tx.execute(
                "INSERT INTO history_chats(conv, more) VALUES ('direct:pn:15550100999', 1)",
                [],
            )
            .map(drop)
            .map_err(sql("test push"))
        })
        .await
        .expect("push");
}

/// W4 (O2): a live message of someone else is noted for the history's unread hint, in the zone that hands
/// it over; our own message from the phone is not (the phone does not count it unread), and a zone that
/// fails notes nothing (the row is handled again whole).
#[tokio::test]
async fn a_live_message_of_another_is_noted_for_the_hint_and_our_own_is_not() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    // A push is open (a chat owes its end): the live path notes (W4 fix F4).
    open_a_push(&store).await;
    let mut ours = arrival_in(PEER, "OWN1", "typed on my phone", 1_700_000_001, None);
    ours.meta.from_me = true;
    commit(
        &w,
        vec![
            arrival_in(PEER, "THEIRS1", "hello", 1_700_000_010, None),
            ours,
            arrival_in(PEER, "THEIRS2", "again", 1_700_000_020, None),
        ],
    )
    .await;
    fault::arm(fault::STAGE2_COMMIT, 1);
    assert!(
        w.stage2.step().await.is_err(),
        "the armed seam fails the zone"
    );
    assert_eq!(
        count(&store, "SELECT count(*) FROM history_live").await,
        0,
        "a rolled-back zone notes nothing"
    );
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.finished, report.events), (3, 3));
    let shown = messages(&store).await;
    assert_eq!(shown.len(), 3, "all three are messages for the core");
    assert!(matches!(
        shown[1].origin,
        kourier_model::Origin::OwnOtherDevice { .. }
    ));
    assert_eq!(
        count(&store, "SELECT count(*) FROM history_live").await,
        2,
        "the two of the other side are noted, ours is not"
    );
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM history_live WHERE conv = 'direct:pn:15550100101' \
             AND sent_at IN (1700000010000, 1700000020000)"
        )
        .await,
        2,
        "by the conversation the core knows and by server time (ms)"
    );
    w.owner.close();
}

/// W4 fix F4: with no push to come, the live path notes no message (a row each, for ever, for a hint nobody states),
/// and the notes of an earlier push are forgotten in the step's own zone.
/// Mutant for the re-check: the `wanted` look removed from `apply` (every message noted).
#[tokio::test]
async fn no_push_no_notes_and_a_step_forgets_the_old_ones() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    // An old note from a push that ended long ago.
    store
        .write("test note", |tx| {
            tx.execute(
                "INSERT INTO history_live(conv, sent_at) VALUES ('direct:pn:15550100101', 1000)",
                [],
            )
            .map(drop)
            .map_err(sql("test note"))
        })
        .await
        .expect("note");
    commit(
        &w,
        vec![arrival_in(PEER, "THEIRS1", "hello", 1_700_000_010, None)],
    )
    .await;
    let report = w.stage2.step().await.expect("step");
    assert_eq!(report.events, 1, "the message itself is delivered");
    assert_eq!(
        count(&store, "SELECT count(*) FROM history_live").await,
        0,
        "no push is open: the new message is not noted, and the old note went with the step"
    );
    w.owner.close();
}

/// T1a (A5): a notice of a history chunk our phone announced is said in ONE fixed line, after the zone that
/// recorded it committed: a zone that fails says nothing, the same id taken up again says nothing, a peer's
/// notice says nothing, and no line holds the id. Mutants: no line; the line inside the failed zone's path
/// (before the commit); a line for an id already recorded.
#[tokio::test]
async fn a_recorded_notice_logs_one_fixed_line_without_an_id() {
    use wa::message::protocol_message::Type;
    let _serial = fault::serial();
    let w = world(quick());
    let notice = {
        let mut inner = wa::message::ProtocolMessage::default();
        inner.r#type = Some(Type::HISTORY_SYNC_NOTIFICATION);
        let mut message = wa::Message::default();
        message.protocol_message = waproto::buffa::MessageField::some(inner);
        message
    };
    let mut ours = arrival_of(PEER, PEER, "CHUNK-LOGGED", &notice);
    ours.meta.from_me = true;
    let theirs = arrival_of(PEER, PEER, "SPOOF-LOGGED", &notice);

    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    commit(&w, vec![ours, theirs]).await;
    fault::arm(fault::STAGE2_COMMIT, 1);
    assert!(
        w.stage2.step().await.is_err(),
        "the armed seam fails the zone"
    );
    assert!(
        lines.containing("history notice recorded").is_empty(),
        "a rolled-back zone said a notice was recorded: {:?}",
        lines.all()
    );

    w.stage2.step().await.expect("step");
    assert_eq!(
        lines.containing("history notice recorded").len(),
        1,
        "only our phone's notice, once: {:?}",
        lines.all()
    );

    let mut again = arrival_of(PEER, PEER, "CHUNK-LOGGED", &notice);
    again.meta.from_me = true;
    again.chat = "15550100102@s.whatsapp.net".into();
    again.sender = again.chat.clone();
    commit(&w, vec![again]).await;
    w.stage2.step().await.expect("step");
    assert_eq!(
        lines.containing("history notice recorded").len(),
        1,
        "an id already recorded says nothing again"
    );
    for line in lines.all() {
        assert!(!line.contains("LOGGED"), "an id in a line: {line}");
    }
    w.owner.close();
}

/// W5a (D7): a message of the peer proves its 1:1 chat started on the phone, noted in the zone that hands the
/// message over: a rolled-back zone notes nothing, the next step notes it once.
#[tokio::test]
async fn a_message_of_the_peer_starts_its_chat_in_the_zone_that_hands_it_over() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(&w, vec![arrival("A", "one")]).await;
    let started = || {
        count(
            &store,
            "SELECT count(*) FROM chat_started WHERE conv = 'direct:pn:15550100101' AND how = 0",
        )
    };
    fault::arm(fault::STAGE2_COMMIT, 1);
    assert!(
        w.stage2.step().await.is_err(),
        "the armed seam fails the zone"
    );
    assert_eq!(started().await, 0, "a rolled-back zone notes no start");
    w.stage2.step().await.expect("step");
    assert_eq!(started().await, 1, "the peer's message started the chat");
    w.owner.close();
}

/// The shape of one `core_outbox` row's events, for the tests of the phone's archive: statements, archive words, messages.
async fn shapes(store: &KourierStore, chat: &kourier_model::ConvKey) -> Vec<Vec<String>> {
    store
        .read(|conn| {
            let rows = outbox::peek(conn, 100).map_err(|_| crate::error::database("peek"))?;
            Ok(rows
                .into_iter()
                .map(|r| decode_outbox_row(&r.envelope).expect("decodes"))
                .collect::<Vec<_>>())
        })
        .await
        .expect("read")
        .iter()
        .map(|row| {
            row.iter()
                .map(|event| match event {
                    BackendEvent::ConversationUpsert(patch)
                        if crate::features::is_statement(event) =>
                    {
                        format!("statement {}", patch.key == *chat)
                    }
                    BackendEvent::ConversationUpsert(patch) => {
                        format!("archived {:?}", patch.archived)
                    }
                    BackendEvent::Message(_) => "message".to_owned(),
                    _ => "other".to_owned(),
                })
                .collect()
        })
        .collect()
}

/// The phone's word on `chat`'s archive (time `action_ms`, stated or not) and on "a new message unarchives".
async fn phone_archived(
    store: &KourierStore,
    chat: &kourier_model::ConvKey,
    action_ms: i64,
    stated: bool,
) {
    use crate::store::chat_state::{self, Field, Put};
    let chat = chat.clone();
    store
        .write("the phone's word", move |tx| {
            chat_state::put(
                tx,
                &Put {
                    chat: &chat,
                    field: Field::Archived,
                    value: 1,
                    action_ms,
                    generation: 0,
                    collection: "regular_low",
                    stated,
                },
            )?;
            chat_state::set_account_flag(tx, chat_state::UNARCHIVE_CHATS, true).map(drop)
        })
        .await
        .expect("mirror");
}

fn pn_chat() -> kourier_model::ConvKey {
    kourier_model::ConvKey::Direct(
        kourier_model::UserId::from_parts("pn", "15550100101").expect("a user"),
    )
}

/// W6a1 (H5, F1): the phone's archive of a chat the core has not heard of waits in the mirror. The zone that hands the
/// chat's first message over notes that message first, so an archive OLDER than it is a stale record of a chat the
/// phone's setting moved out: the chat is stated, not archived, with no archive word at all. Mutant: stage 2 notes
/// nothing before it states the chat (the chat is archived and brought back in one row).
#[tokio::test]
async fn an_archive_older_than_the_first_message_of_a_chat_is_never_stated() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let chat = pn_chat();
    // The archive is from 10 ms; the message is from 1_700_000_000 s.
    phone_archived(&store, &chat, 10, false).await;
    commit(&w, vec![arrival("A", "one")]).await;
    w.stage2.step().await.expect("step");
    assert_eq!(
        shapes(&store, &chat).await,
        [["statement true", "message"]],
        "one row: the statement and the message"
    );
    w.owner.close();
}

/// W6a1 (H6, F1): a message NEWER than the phone's archive of a chat the core knows brings the chat back in its own row,
/// in front of the message, when the phone's "keep chats archived" is off. Mutant: stage 2 states its chats without the
/// unarchive step.
#[tokio::test]
async fn a_message_newer_than_the_archive_brings_the_chat_back_in_its_own_row_when_the_phones_setting_says_so()
 {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let chat = pn_chat();
    commit(&w, vec![arrival("A", "one")]).await;
    w.stage2.step().await.expect("step");
    // The user archives the chat after message A: the archive's range ends at A (seconds, times 1000).
    phone_archived(&store, &chat, 1_700_000_000_000, true).await;
    commit(&w, vec![arrival_in(PEER, "B", "two", 1_700_000_050, None)]).await;
    w.stage2.step().await.expect("step");
    assert_eq!(
        shapes(&store, &chat).await,
        [
            vec!["statement true", "message"],
            vec!["archived Some(false)", "message"]
        ],
        "the second row brings the chat back in front of its message"
    );
    w.owner.close();
}

/// W5a, D9: a chat's features are stated in the zone that hands its first message over, with the started fact
/// of that same message, and once: the next row of the chat carries none.
#[tokio::test]
async fn a_chat_is_stated_once_in_the_row_of_its_first_message() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(&w, vec![arrival("A", "one")]).await;
    w.stage2.step().await.expect("step");
    commit(&w, vec![arrival("B", "two")]).await;
    w.stage2.step().await.expect("step");
    let rows = store
        .read(|conn| {
            let rows = outbox::peek(conn, 100).map_err(|_| crate::error::database("peek"))?;
            Ok(rows
                .into_iter()
                .map(|r| decode_outbox_row(&r.envelope).expect("decodes"))
                .collect::<Vec<_>>())
        })
        .await
        .expect("read");
    assert_eq!(rows.len(), 2, "one row per inbox row");
    let statement = |row: &[BackendEvent]| {
        row.iter()
            .filter(|e| crate::features::is_statement(e))
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(patch) => patch.features.clone(),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let first = statement(&rows[0]);
    assert_eq!(first.len(), 1, "the first row states the chat");
    assert_eq!(
        first[0].cannot_send, None,
        "the peer's message started it in the same zone"
    );
    assert!(
        crate::features::is_statement(&rows[0][0]),
        "before the message"
    );
    assert!(statement(&rows[1]).is_empty(), "stated once");
    w.owner.close();
}

// ---- W6a2, B7a: an admin's delete waits for the group's member list ----

const GROUP_JID: &str = "120363000071529611@g.us";
const GROUP_OTHER_JID: &str = "120363000071529622@g.us";
const ADMIN: &str = "99900071501:3@lid";
const MEMBER: &str = "15550071502@s.whatsapp.net";

fn group_key(jid: &str) -> kourier_model::ConvKey {
    kourier_model::ConvKey::Group(format!("wag:{}", jid.trim_end_matches("@g.us")).into())
}

/// `sender`'s revoke in `chat` of the message `target` written by `participant`.
fn revoke_in(
    chat: &str,
    sender: &str,
    id: &str,
    target: &str,
    participant: Option<&str>,
) -> Arrival {
    use waproto::buffa::MessageField;
    let mut key = wa::MessageKey::default();
    key.id = Some(target.into());
    key.from_me = Some(false);
    key.participant = participant.map(str::to_owned);
    let mut protocol = wa::message::ProtocolMessage::default();
    protocol.r#type = Some(wa::message::protocol_message::Type::REVOKE);
    protocol.key = MessageField::some(key);
    let mut message = wa::Message::default();
    message.protocol_message = MessageField::some(protocol);
    let mut arrival = arrival_of(chat, sender, id, &message);
    arrival.meta.is_group = true;
    arrival
}

/// One inbox row's park, as the rows hold it: (state, attempts, parked_until, members_wait).
async fn parked(store: &KourierStore, id: &'static str) -> (String, i64, i64, Option<String>) {
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT state, attempts, parked_until, members_wait FROM inbox WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(sql("read a park"))
        })
        .await
        .expect("row")
}

async fn state_members(store: &KourierStore, group: &str) {
    let key = group_key(group);
    store
        .write("test: state a group's members", move |tx| {
            crate::store::people::put_group(
                tx,
                &key,
                &crate::groups::State {
                    members_stated: true,
                    ..crate::groups::State::default()
                },
            )
        })
        .await
        .expect("state");
}

/// D11 + M2 + the map step: an admin's delete that W3b deferred at map version 3 is taken up at version 4 and WAITS
/// (state `new`, parked until its arrival + 45 days, marked with its group, NO attempt counted, no event); the group of ANOTHER
/// lane is not held up by it; when the zone that states the group's members lets the lane go, the next step maps it to a
/// `Deleted` by the admin.
/// Mutants: the delete is mapped before the member list is stated; a poison attempt is counted while it waits; the release
/// leaves the park.
#[tokio::test]
async fn an_admins_delete_waits_without_an_attempt_and_is_mapped_after_the_members_are_stated() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(
        &w,
        vec![
            revoke_in(GROUP_JID, ADMIN, "REV1", "TARGET1", Some(MEMBER)),
            arrival_of(
                GROUP_OTHER_JID,
                ADMIN,
                "OTHER1",
                &text("an ordinary message"),
            ),
        ],
    )
    .await;
    // The state W3b left the revoke in: deferred by map version 3.
    store
        .write("plant W3b's state", |tx| {
            tx.execute(
                "UPDATE inbox SET state = 'deferred', map_version = 3 WHERE id = 'REV1'",
                [],
            )
            .map_err(sql("plant"))
            .map(drop)
        })
        .await
        .expect("plant");
    assert_eq!(w.stage2.take_up_deferred().await.expect("take up"), 1);
    let report = w.stage2.step().await.expect("step");
    assert_eq!(
        (
            report.taken,
            report.waiting,
            report.events,
            report.parked,
            report.poisoned,
            report.deferred
        ),
        (2, 1, 1, 0, 0, 0),
        "the revoke waits, the ordinary message of the other group is delivered: {report:?}"
    );
    let (state, attempts, until, group) = parked(&store, "REV1").await;
    assert_eq!(state, "new");
    assert_eq!(attempts, 0, "waiting is no failure");
    assert_eq!(until, i64::try_from(START).expect("ms") + super::MAX_AGE_MS);
    assert_eq!(
        group.as_deref(),
        Some(group_key(GROUP_JID).encode().as_str())
    );
    assert_eq!(
        w.stage2.step().await.expect("step").taken,
        0,
        "a parked row is not looked at again"
    );
    assert_eq!(event_names(&store).await, ["Message"]);

    // The zone that states the members lets the lane go.
    state_members(&store, GROUP_JID).await;
    let released = store
        .write("test: release", |tx| {
            crate::store::people::release_waiting(tx, &group_key(GROUP_JID))
        })
        .await
        .expect("release");
    assert_eq!(released, 1);
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.taken, report.events, report.finished), (1, 1, 1));
    assert_eq!(event_names(&store).await, ["Message", "Deleted"]);
    let (state, _, _, group) = parked(&store, "REV1").await;
    assert_eq!((state.as_str(), group), ("done", None));
    w.owner.close();
}

/// M2: the 45 days of a waiting delete count from its ARRIVAL, never from the take-up of a newer map (which sets
/// `aged_from`): a delete that arrived 44 days ago waits one more day, not 45; one older than 45 days is finished at its
/// first step with no event; and the day a waiting delete reaches its 45th it is finished with no event (the core could
/// not judge it for that long, and it is not kept for ever).
/// Mutants: the bound counts from `aged_from`; a row past the bound is parked again.
#[tokio::test]
async fn a_waiting_deletes_45_days_count_from_its_arrival_and_end_it_with_no_event() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    let day: u64 = 24 * 3600 * 1000;
    w.clock.set(START - 46 * day);
    commit(
        &w,
        vec![revoke_in(
            GROUP_JID,
            ADMIN,
            "REVOLD",
            "TARGET1",
            Some(MEMBER),
        )],
    )
    .await;
    w.clock.set(START - 44 * day);
    commit(
        &w,
        vec![revoke_in(
            GROUP_JID,
            ADMIN,
            "REVNEW",
            "TARGET2",
            Some(MEMBER),
        )],
    )
    .await;
    w.clock.set(START);
    // A newer map took both up just now.
    let now = i64::try_from(START).expect("ms");
    store
        .write("plant a take-up", move |tx| {
            tx.execute("UPDATE inbox SET aged_from = ?1", [now])
                .map_err(sql("plant"))
                .map(drop)
        })
        .await
        .expect("plant");
    let report = w.stage2.step().await.expect("step");
    assert_eq!(
        (report.taken, report.waiting, report.ended, report.events),
        (2, 1, 1, 0),
        "{report:?}"
    );
    let (state, _, _, group) = parked(&store, "REVOLD").await;
    assert_eq!(
        (state.as_str(), group),
        ("done", None),
        "older than 45 days: finished"
    );
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM inbox WHERE id = 'REVOLD' AND message IS NULL AND meta IS NULL"
        )
        .await,
        1,
        "and wiped"
    );
    let (state, attempts, until, _) = parked(&store, "REVNEW").await;
    assert_eq!((state.as_str(), attempts), ("new", 0));
    assert_eq!(
        until,
        now + i64::try_from(day).expect("day"),
        "arrival + 45 days = one more day, not 45 from the take-up"
    );
    // The day comes.
    w.clock.advance(Duration::from_millis(day));
    let report = w.stage2.step().await.expect("step");
    assert_eq!(
        (report.taken, report.ended, report.waiting, report.events),
        (1, 1, 0, 0),
        "{report:?}"
    );
    assert_eq!(parked(&store, "REVNEW").await.0, "done");
    assert!(event_names(&store).await.is_empty(), "no event for either");
    w.owner.close();
}

/// The member list stated between the mapper's read of the facts and the zone (one writer: the order is total): the zone
/// looks at the group's row again and parks nothing, so the lane's release (which came first) cannot be missed. A group the
/// answers say we are not in ends the delete with no event.
/// Mutants: the zone parks without looking at the row; the zone ignores `not_member`.
#[tokio::test]
async fn the_zone_looks_at_the_groups_row_again_before_it_parks() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(
        &w,
        vec![
            revoke_in(GROUP_JID, ADMIN, "REV1", "TARGET1", Some(MEMBER)),
            revoke_in(GROUP_OTHER_JID, ADMIN, "REV2", "TARGET2", Some(MEMBER)),
        ],
    )
    .await;
    // The mapper's answer (made before the members were stated) and then the state of the rows.
    let now = i64::try_from(START).expect("ms");
    let rows = store
        .read(move |conn| super::load(conn, now))
        .await
        .expect("rows");
    let plan: Vec<super::Planned> = rows
        .into_iter()
        .map(|row| {
            let group = group_key(if row.id == "REV1" {
                GROUP_JID
            } else {
                GROUP_OTHER_JID
            });
            super::Planned::new(row, Ok(crate::map::Mapped::waiting_for_members(group)), now)
        })
        .collect();
    state_members(&store, GROUP_JID).await;
    let gone = group_key(GROUP_OTHER_JID);
    store
        .write("test: we left", move |tx| {
            crate::store::people::put_group(
                tx,
                &gone,
                &crate::groups::State {
                    not_member: true,
                    ..crate::groups::State::default()
                },
            )
        })
        .await
        .expect("left");
    let report = store
        .write_at("test zone", super::POINTS, move |tx| {
            super::apply(tx, now, &plan)
        })
        .await
        .expect("the zone");
    assert_eq!(
        (report.waiting, report.ended, report.finished, report.events),
        (0, 1, 1, 0),
        "{report:?}"
    );
    assert_eq!(
        parked(&store, "REV1").await,
        ("new".to_owned(), 0, 0, None),
        "left as it was: the next step maps it with the new facts"
    );
    assert_eq!(parked(&store, "REV2").await.0, "done");
    // And that next step does map it.
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.taken, report.events, report.waiting), (1, 1, 0));
    assert_eq!(event_names(&store).await, ["Deleted"]);
    w.owner.close();
}

/// M3 through the store: a member's delete of his own message under his other id is mapped at once (the member list not
/// stated), whichever way the pair is stored (each in a step of its own: the facts of a step are shared by its rows); with no
/// stored pair the same revoke is an admin's and waits.
/// Mutants: the number's LIDs are read by the wrong column; the LID's number is not read.
#[tokio::test]
async fn a_members_delete_under_his_other_id_is_mapped_by_the_stores_pair() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    store
        .write("test: a pair", |tx| {
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES ('99900071501', '15550071501', 1, 1, 'test')",
                [],
            )
            .map_err(sql("pair"))
            .map(drop)
        })
        .await
        .expect("pair");
    // He sends by his LID; the key names his number. The number of somebody else is an admin's delete.
    commit(
        &w,
        vec![
            revoke_in(
                GROUP_JID,
                ADMIN,
                "REVA",
                "TARGET1",
                Some("15550071501@s.whatsapp.net"),
            ),
            revoke_in(
                GROUP_JID,
                ADMIN,
                "REVC",
                "TARGET3",
                Some("15550071509@s.whatsapp.net"),
            ),
        ],
    )
    .await;
    let report = w.stage2.step().await.expect("step");
    assert_eq!(
        (report.taken, report.events, report.waiting),
        (2, 1, 1),
        "{report:?}"
    );
    assert_eq!(parked(&store, "REVA").await.0, "done");
    assert_eq!(parked(&store, "REVC").await.0, "new");
    // He sends by his number; the key names his LID.
    commit(
        &w,
        vec![revoke_in(
            GROUP_JID,
            "15550071501:2@s.whatsapp.net",
            "REVB",
            "TARGET2",
            Some("99900071501@lid"),
        )],
    )
    .await;
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.taken, report.events, report.waiting), (1, 1, 0));
    assert_eq!(parked(&store, "REVB").await.0, "done");
    assert_eq!(event_names(&store).await, ["Deleted", "Deleted"]);
    w.owner.close();
}

/// The rows of `core_outbox` as the names of their events: a contact's name as `name <text>`, a message as `message`.
async fn name_rows(store: &KourierStore) -> Vec<Vec<String>> {
    store
        .read(|conn| {
            let rows = outbox::peek(conn, 1_000).map_err(|_| crate::error::database("peek"))?;
            Ok(rows
                .into_iter()
                .map(|r| decode_outbox_row(&r.envelope).expect("decodes"))
                .map(|row| {
                    row.into_iter()
                        .filter_map(|event| match event {
                            BackendEvent::ContactUpsert(patch) => match patch.profile_name {
                                kourier_model::Patch::Set(name) => Some(format!("name {name}")),
                                _ => Some("contact".to_owned()),
                            },
                            BackendEvent::Message(_) => Some("message".to_owned()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .collect())
        })
        .await
        .expect("read")
}

fn said_by(push: &str, id: &str, from_me: bool) -> Arrival {
    let mut a = arrival_in(PEER, id, "hello", 1_700_000_010, None);
    a.meta.push_name = push.to_owned();
    a.meta.from_me = from_me;
    a
}

/// W6a2 B8a (D8): a sender's push name is stated in the row of his message, after the merges and before the message, only
/// when it differs from what the core was told; the placeholder and our own name are never stated; a zone that fails tells
/// nothing (the next step states it); the step's one line counts and holds no name. Mutants: a name on every message; "-"
/// stated; our own name stated; the record written outside the zone's commit; the name after the message.
#[tokio::test]
async fn a_senders_push_name_is_stated_in_the_row_of_his_message_on_a_change_only() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    commit(
        &w,
        vec![
            said_by("Ann Example", "M1", false),
            said_by("Ann Example", "M2", false),
            said_by("Ann Renamed", "M3", false),
            said_by("-", "M4", false),
            said_by("My Own Name", "M5", true),
        ],
    )
    .await;
    let lines = crate::test_log::Lines::default();
    let _guard = tracing::subscriber::set_default(lines.clone());
    fault::arm(fault::STAGE2_COMMIT, 1);
    assert!(
        w.stage2.step().await.is_err(),
        "the armed seam fails the zone"
    );
    assert_eq!(
        count(&store, "SELECT count(*) FROM names_stated").await,
        0,
        "a rolled-back zone records no name"
    );
    let report = w.stage2.step().await.expect("step");
    assert_eq!((report.finished, report.names), (5, 2));
    assert_eq!(
        name_rows(&store).await,
        vec![
            vec!["name Ann Example".to_owned(), "message".to_owned()],
            vec!["message".to_owned()],
            vec!["name Ann Renamed".to_owned(), "message".to_owned()],
            vec!["message".to_owned()],
            vec!["message".to_owned()],
        ],
        "a name in front of the message of the change, nowhere else"
    );
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM names_stated WHERE person = 'pn:15550100101' AND kind = 'profile' \
             AND value = 'Ann Renamed'"
        )
        .await,
        1,
        "the last name told is the row"
    );
    assert_eq!(count(&store, "SELECT count(*) FROM names_stated").await, 1);
    for line in lines.all() {
        assert!(
            !line.contains("Ann") && !line.contains("My Own"),
            "a name in a line: {line}"
        );
    }
    w.owner.close();
}

/// W6a2 B8a (F3 of W4): while a push is open the live name is noted for the history delivery, in the SAME zone as the
/// statement, so an older chunk does not overwrite it; without a push nothing is noted. Mutant: the note taken from the
/// map's events (without the name).
#[tokio::test]
async fn a_live_push_name_is_noted_for_the_history_while_a_push_is_open() {
    let _serial = fault::serial();
    let w = world(quick());
    let store = w.owner.store();
    open_a_push(&store).await;
    commit(&w, vec![said_by("Ann Example", "M1", false)]).await;
    w.stage2.step().await.expect("step");
    assert_eq!(
        count(
            &store,
            "SELECT count(*) FROM history_named WHERE kind = 1 AND key = 'pn:15550100101'"
        )
        .await,
        1,
        "the name the live path stated is noted"
    );
    w.owner.close();
}
