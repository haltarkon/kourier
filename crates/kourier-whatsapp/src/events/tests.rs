//! The handler's durable writes (W3a, chunk C5): the wait for the commit, the group commit, a failed
//! write, the order against the detached inbox writes. The handler itself (translation, the failure
//! path that ends the connection) is chunk C6; the kill loop over the Wire is C10.
#![allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]

use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, FakeClock, decode_outbox_row};
use kourier_db::{DbKey, Priority, outbox};
use kourier_model::ConvKey;
use tokio::sync::Notify;

use super::{EventWriter, WriteFailed};
use crate::error::sql;
use crate::fault;
use crate::inbox::{Arrival, Inbox, InboxMeta, Origin};
use crate::store::{KourierStore, StoreOwner};

struct World {
    _dir: tempfile::TempDir,
    owner: StoreOwner,
    writer: EventWriter,
    wake: Arc<Notify>,
}

fn world(max_group: usize) -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([8; 32]),
        "wa-events",
    )
    .expect("open");
    let wake = Arc::new(Notify::new());
    let writer = EventWriter::with_limit(
        owner.store(),
        Arc::new(FakeClock::new(1_000_000)),
        wake.clone(),
        max_group,
    );
    World {
        _dir: dir,
        owner,
        writer,
        wake,
    }
}

/// An event that names its number.
fn event(n: usize) -> BackendEvent {
    BackendEvent::ConversationRemoved {
        conv: ConvKey::Group(format!("wag:{n}").into()),
    }
}

/// The numbers of the events in `core_outbox`, in row order.
async fn written(store: &KourierStore) -> Vec<usize> {
    store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut out = Vec::new();
            for row in rows {
                for event in decode_outbox_row(&row.envelope).expect("decodes") {
                    if let BackendEvent::ConversationRemoved {
                        conv: ConvKey::Group(name),
                    } = event
                    {
                        out.push(
                            name.to_string()
                                .trim_start_matches("wag:")
                                .parse()
                                .expect("n"),
                        );
                    }
                }
            }
            Ok(out)
        })
        .await
        .expect("read the outbox")
}

/// Parks the store's writer thread until the returned sender is used: every job queued meanwhile waits.
async fn park(store: &KourierStore) -> mpsc::Sender<()> {
    let (release, wait) = mpsc::channel::<()>();
    let (started_tx, started) = mpsc::channel::<()>();
    let store = store.clone();
    tokio::spawn(async move {
        let _ = store
            .raw(Priority::High, move |_| {
                let _ = started_tx.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await;
    });
    for _ in 0..500 {
        if started.try_recv().is_ok() {
            return release;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the writer did not park");
}

/// A handler thread: blocks in `write_and_wait`, reports the answer.
fn handler(
    writer: &EventWriter,
    n: usize,
) -> (JoinHandle<()>, mpsc::Receiver<Result<(), WriteFailed>>) {
    let writer = writer.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = tx.send(writer.write_and_wait(&[event(n)]));
    });
    (thread, rx)
}

/// Waits (without blocking the runtime) until `n` rows are buffered.
async fn until_buffered(writer: &EventWriter, n: usize) {
    for _ in 0..500 {
        if writer.buffered() == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} rows buffered, wanted {n}", writer.buffered());
}

#[tokio::test]
async fn the_handler_waits_for_the_commit() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let release = park(&store).await;
    let (thread, answer) = handler(&w.writer, 1);
    until_buffered(&w.writer, 1).await;
    // Parked writer: nothing can have been committed, so the handler must still be inside.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "the handler returned before its row was committed: the ack would go out for a lost event"
    );
    release.send(()).expect("release");
    thread.join().expect("handler thread");
    assert_eq!(answer.recv().expect("answer"), Ok(()));
    assert_eq!(written(&store).await, [1]);
    w.owner.close();
}

#[tokio::test]
async fn the_waiters_of_one_group_are_released_by_one_commit_in_buffer_order() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let release = park(&store).await;
    let mut threads = Vec::new();
    for n in 1..=6 {
        threads.push(handler(&w.writer, n));
        // One at a time, so the buffer order is known.
        until_buffered(&w.writer, n).await;
    }
    release.send(()).expect("release");
    for (thread, answer) in threads {
        thread.join().expect("handler thread");
        assert_eq!(answer.recv().expect("answer"), Ok(()));
    }
    assert_eq!(w.writer.commits(), 1, "six rows, one commit");
    assert_eq!(written(&store).await, [1, 2, 3, 4, 5, 6], "FIFO");
    w.owner.close();
}

#[tokio::test]
async fn a_group_has_a_bound_and_the_rest_is_the_next_job() {
    let _serial = fault::serial();
    let w = world(2);
    let store = w.owner.store();
    let release = park(&store).await;
    let mut threads = Vec::new();
    for n in 1..=5 {
        threads.push(handler(&w.writer, n));
        until_buffered(&w.writer, n).await;
    }
    release.send(()).expect("release");
    for (thread, answer) in threads {
        // A rest that was never queued would leave its waiter here for ever.
        let said = answer.recv_timeout(Duration::from_secs(20));
        assert_eq!(said, Ok(Ok(())), "a waiter was left behind");
        thread.join().expect("handler thread");
    }
    assert_eq!(w.writer.commits(), 3, "two, two and one");
    assert_eq!(written(&store).await, [1, 2, 3, 4, 5]);
    w.owner.close();
}

#[tokio::test]
async fn a_failed_group_fails_every_waiter_and_writes_nothing() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    fault::arm(fault::EVENT_WRITE, 1);
    let release = park(&store).await;
    let mut threads = Vec::new();
    for n in 1..=3 {
        threads.push(handler(&w.writer, n));
        until_buffered(&w.writer, n).await;
    }
    release.send(()).expect("release");
    for (thread, answer) in threads {
        thread.join().expect("handler thread");
        assert_eq!(
            answer.recv().expect("answer"),
            Err(WriteFailed::Zone),
            "a handler that is told Ok for a row that was not written acknowledges a lost event"
        );
    }
    assert_eq!(
        written(&store).await,
        Vec::<usize>::new(),
        "nothing of the group"
    );
    assert_eq!(w.writer.commits(), 0);
    // The server sends the stanzas again: the next group goes through.
    let (thread, answer) = handler(&w.writer, 4);
    thread.join().expect("handler thread");
    assert_eq!(answer.recv().expect("answer"), Ok(()));
    assert_eq!(written(&store).await, [4]);
    w.owner.close();
}

#[tokio::test]
async fn a_closed_store_fails_the_handler_at_once() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    w.owner.close();
    let writer = w.writer.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = tx.send(writer.write_and_wait(&[event(1)]));
    });
    let said = rx.recv_timeout(Duration::from_secs(20));
    assert_eq!(
        said,
        Ok(Err(WriteFailed::Closed)),
        "the handler hung or was told Ok"
    );
    thread.join().expect("handler thread");
    drop(store);
}

#[tokio::test]
async fn the_forwarder_is_woken_by_a_commit() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let (thread, answer) = handler(&w.writer, 1);
    thread.join().expect("handler thread");
    assert_eq!(answer.recv().expect("answer"), Ok(()));
    // `notify_one` stores a permit when nobody waits: the forwarder that asks later is not missed.
    tokio::time::timeout(Duration::from_secs(5), w.wake.notified())
        .await
        .expect("the forwarder was not woken");
    w.owner.close();
}

fn newsletter(id: &str) -> Arrival {
    Arrival {
        chat: "120363000000000001@newsletter".into(),
        sender: "120363000000000001@newsletter".into(),
        id: id.into(),
        origin: Origin::Newsletter,
        message: vec![1, 2, 3],
        meta: InboxMeta {
            v: InboxMeta::VERSION,
            timestamp: 1_700_000_000,
            from_me: false,
            is_group: false,
            push_name: String::new(),
            lid_addressing: false,
            sender_alt: None,
            recipient_alt: None,
            ephemeral_expiration: None,
            is_offline: false,
        },
    }
}

/// A newsletter or PDO message is written without a wait (upstream acknowledged it before the handler
/// ran), but in the order of the calls: a detached inbox write queued before a waiting event write is
/// in the file before it, and returns before either has run.
#[tokio::test]
async fn a_detached_inbox_write_does_not_wait_and_keeps_its_place_in_the_queue() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    store
        .raw(Priority::High, |conn| {
            conn.execute_batch(
                "CREATE TABLE probe(inbox_rows INTEGER NOT NULL); \
                 CREATE TRIGGER probe_after_append AFTER INSERT ON core_outbox \
                 BEGIN INSERT INTO probe SELECT count(*) FROM inbox; END;",
            )
            .map_err(sql("create the probe"))
        })
        .await
        .expect("probe");
    let inbox = Inbox::new(store.clone(), Arc::new(FakeClock::new(1_000_000)));
    let release = park(&store).await;
    inbox
        .commit_detached(vec![newsletter("N1")])
        .expect("queued while the writer is parked");
    // Nothing ran: the call returned because the job is QUEUED.
    let (thread, answer) = handler(&w.writer, 1);
    until_buffered(&w.writer, 1).await;
    release.send(()).expect("release");
    thread.join().expect("handler thread");
    assert_eq!(answer.recv().expect("answer"), Ok(()));
    let seen: Vec<i64> = store
        .read(|conn| {
            crate::store::rows(
                conn,
                "read the probe",
                "SELECT inbox_rows FROM probe",
                [],
                |r| r.get(0),
            )
        })
        .await
        .expect("probe rows");
    assert_eq!(
        seen,
        [1],
        "the event write was committed before the detached inbox write that was queued ahead of it"
    );
    w.owner.close();
}

/// Fix F5: while a link has no account the write waits for the hold before it queues anything; a wait
/// that runs out writes nothing and says `Held`; a hold that opens meanwhile lets it through.
#[tokio::test]
async fn a_write_waits_for_the_link_hold_and_fails_held_when_it_stays_closed() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    store.hold().set_closed(true);
    w.writer.set_patience(Duration::from_millis(50));
    let (thread, answer) = handler(&w.writer, 1);
    thread.join().expect("handler thread");
    assert_eq!(answer.recv().expect("answer"), Err(WriteFailed::Held));
    assert_eq!(w.writer.buffered(), 0, "nothing was queued behind the hold");
    assert!(written(&store).await.is_empty());

    // Opened while a handler waits: it goes through, with no time spent beyond the opening.
    w.writer.set_patience(Duration::from_secs(60));
    let (thread, answer) = handler(&w.writer, 2);
    for _ in 0..500 {
        if store.hold().parked() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(store.hold().parked(), 1, "the handler never parked");
    assert!(matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)));
    store.hold().set_closed(false);
    thread.join().expect("handler thread");
    assert_eq!(answer.recv().expect("answer"), Ok(()));
    assert_eq!(written(&store).await, [2]);
    w.owner.close();
}

/// W4 fix F3: the handler's group commit notes the titles and push names its events state (the group notice of a
/// rename), while a push can still state them, and notes nothing without one.
/// Mutant for the re-check: `named::record` removed from the drain.
#[tokio::test]
async fn the_group_commit_notes_a_title_it_states_only_while_a_push_is_open() {
    use kourier_model::{ConversationPatch, Patch};
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let rename = |name: &str| {
        let mut patch = ConversationPatch::new(ConvKey::Group("wag:1".into()));
        patch.title = Patch::Set(name.to_owned());
        BackendEvent::ConversationUpsert(patch)
    };
    let named = |store: &KourierStore| {
        let store = store.clone();
        async move {
            store
                .read(|conn| {
                    conn.query_row("SELECT count(*) FROM history_named", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .map_err(sql("count"))
                })
                .await
                .expect("count")
        }
    };
    let writer = w.writer.clone();
    let first = rename("Trip");
    std::thread::spawn(move || writer.write_and_wait(&[first]))
        .join()
        .expect("handler")
        .expect("written");
    assert_eq!(named(&store).await, 0, "no push is open: nothing is noted");

    store
        .write("test push", |tx| {
            tx.execute(
                "INSERT INTO history_chats(conv, more) VALUES ('group:wag:2', 1)",
                [],
            )
            .map(drop)
            .map_err(sql("test push"))
        })
        .await
        .expect("push");
    let writer = w.writer.clone();
    let second = rename("Trip, final");
    std::thread::spawn(move || writer.write_and_wait(&[second]))
        .join()
        .expect("handler")
        .expect("written");
    assert_eq!(named(&store).await, 1, "a push is open: the title is noted");
    w.owner.close();
}

/// W6a2 (B5): a group's notice is written in ONE zone with the group's row: the acts are made into events against it, the
/// group's features are stated BEFORE its first event (M4: a member delta never reaches the core ahead of the features),
/// the row follows, and the title it sets is noted for the history while a push is open. A notice whose acts change
/// nothing writes just its line.
/// Mutant: the notice entry skips `zone::prepare` (members without features).
#[tokio::test]
async fn a_groups_notice_is_written_with_its_row_and_the_features_come_first() {
    use crate::groups::{Rules, State};
    use crate::notice::{Act, Noticed};
    use crate::store::people;
    use kourier_model::{Member, MemberRole, Patch, UserId};

    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let conv = ConvKey::Group("wag:120363000071529611".into());
    let ada = UserId::from_parts("lid", "99000071522").expect("a user");
    store
        .write("test: seed a group and a push", {
            let conv = conv.clone();
            move |tx| {
                people::put_group(
                    tx,
                    &conv,
                    &State {
                        members_stated: true,
                        rules: Some(Rules::default()),
                        ..State::default()
                    },
                )?;
                tx.execute(
                    "INSERT INTO history_chats(conv, more) VALUES ('group:wag:2', 1)",
                    [],
                )
                .map(drop)
                .map_err(sql("test push"))
            }
        })
        .await
        .expect("seed");
    let notice = Noticed {
        conv: conv.clone(),
        events: Vec::new(),
        acts: vec![
            Act::Members {
                upsert: vec![Member {
                    user: ada,
                    role: MemberRole::Admin,
                }],
                remove: Vec::new(),
            },
            Act::Title {
                title: "A synthetic name".to_owned(),
                at_s: 1_700_000_000,
            },
        ],
    };
    let writer = w.writer.clone();
    std::thread::spawn(move || writer.write_notice_and_wait(notice))
        .join()
        .expect("handler")
        .expect("written");
    let rows = store
        .read(|conn| outbox::peek(conn, 100).map_err(|_| crate::error::database("peek")))
        .await
        .expect("rows");
    assert_eq!(rows.len(), 1, "one row");
    let events = decode_outbox_row(&rows[0].envelope).expect("decodes");
    let [
        BackendEvent::ConversationUpsert(first),
        BackendEvent::ConversationUpsert(second),
    ] = events.as_slice()
    else {
        panic!("a statement, then the notice's upsert: {events:?}")
    };
    assert!(
        crate::features::is_statement(&events[0]),
        "the features come first"
    );
    assert!(first.members.is_none());
    assert_eq!(second.title, Patch::Set("A synthetic name".to_owned()));
    assert!(second.members.is_some() && second.features.is_some());
    // The row followed, and the title is noted for the history (a push is open).
    let state = store
        .read({
            let conv = conv.clone();
            move |conn| people::group(conn, &conv)
        })
        .await
        .expect("row");
    assert_eq!(state.subject_time, Some(1_700_000_000));
    let noted = store
        .read(|conn| {
            conn.query_row("SELECT count(*) FROM history_named", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(sql("count"))
        })
        .await
        .expect("count");
    assert_eq!(noted, 1);
    w.owner.close();
}

/// W4 fix F7: a row that says a chat was read on the phone notes it in the zone that writes it, so the history states
/// no hint for that chat afterwards; a plain write notes nothing.
/// Mutant for the re-check: the `reads` loop removed from the drain.
#[tokio::test]
async fn a_read_on_the_phone_is_noted_in_the_zone_that_writes_its_row() {
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let chat =
        ConvKey::Direct(kourier_model::UserId::from_parts("pn", "15550100101").expect("user"));
    let read_here = |store: &KourierStore, chat: &ConvKey| {
        let (store, key) = (store.clone(), chat.encode());
        async move {
            store
                .read(move |conn| {
                    conn.query_row(
                        "SELECT count(*) FROM history_chats WHERE conv = ?1 AND read_here = 1",
                        [key],
                        |r| r.get::<_, i64>(0),
                    )
                    .map_err(sql("count"))
                })
                .await
                .expect("count")
        }
    };
    let writer = w.writer.clone();
    std::thread::spawn(move || writer.write_and_wait(&[event(1)]))
        .join()
        .expect("handler")
        .expect("written");
    assert_eq!(
        read_here(&store, &chat).await,
        0,
        "a plain write notes no read"
    );
    let writer = w.writer.clone();
    let noted = chat.clone();
    std::thread::spawn(move || writer.write_and_wait_reading(&[event(2)], &[noted]))
        .join()
        .expect("handler")
        .expect("written");
    assert_eq!(read_here(&store, &chat).await, 1);
    assert_eq!(written(&store).await, [1, 2]);
    w.owner.close();
}

/// W6a1 (H6): the writer's zone is a zone of messages too (a call line, an undecryptable placeholder): a CALL LINE counts
/// as a message that brings an archived chat back when the phone's setting says so, in the row of the line; a notice or
/// a placeholder does not.
/// Mutant: the drain states its chats without the unarchive step.
#[tokio::test]
async fn a_call_line_brings_an_archived_chat_back_in_the_group_commit() {
    use kourier_backend_api::IncomingMessage;
    use kourier_model::{Body, CallDirection, CallLine, CallState, Origin, SystemEvent, UserId};
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let caller = UserId::from_parts("pn", "15550192736481").expect("a user");
    let chat = ConvKey::Direct(caller.clone());
    {
        let chat = chat.clone();
        store
            .write("the phone's word", move |tx| {
                use crate::store::chat_state::{self, Field, Put};
                crate::send::started::mark_stated(tx, &chat, crate::features::VERSION, false)?;
                chat_state::put(
                    tx,
                    &Put {
                        chat: &chat,
                        field: Field::Archived,
                        value: 1,
                        action_ms: 5,
                        generation: 0,
                        collection: "regular_low",
                        stated: true,
                    },
                )?;
                chat_state::set_account_flag(tx, chat_state::UNARCHIVE_CHATS, true).map(drop)
            })
            .await
            .expect("mirror");
    }
    let line = |id: &str, body: Body| {
        BackendEvent::Message(IncomingMessage::new(
            chat.clone(),
            id,
            Origin::Remote {
                author: caller.clone(),
                device: None,
            },
            body,
            1_000,
            1_000,
        ))
    };
    let call = line(
        "CALL1",
        Body::System(SystemEvent::Call(CallLine::signal(
            CallDirection::Incoming,
            false,
            CallState::Missed,
            1_000,
        ))),
    );
    let writer = w.writer.clone();
    let events = vec![line("JOIN1", Body::System(SystemEvent::Joined))];
    std::thread::spawn(move || writer.write_and_wait(&events))
        .join()
        .expect("handler thread")
        .expect("written");
    let writer = w.writer.clone();
    let events = vec![call.clone()];
    std::thread::spawn(move || writer.write_and_wait(&events))
        .join()
        .expect("handler thread")
        .expect("written");
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
    let archived = |row: &[BackendEvent]| -> Vec<Option<bool>> {
        row.iter()
            .filter_map(|e| match e {
                BackendEvent::ConversationUpsert(p) if p.archived.is_some() => Some(p.archived),
                _ => None,
            })
            .collect()
    };
    assert_eq!(rows.len(), 2);
    assert!(
        archived(&rows[0]).is_empty(),
        "a notice brings nothing back"
    );
    assert_eq!(archived(&rows[1]), [Some(false)], "a call line does");
    assert_eq!(rows[1].last(), Some(&call), "in front of the line");
    w.owner.close();
}

/// W5a, D7/D9: a call line goes through the writer, and the writer's zone states its chat's features with it: a
/// stranger's call is no start, so the chat states `NotAccepted` (and the send path refuses it, `send::attempt`).
#[tokio::test]
async fn a_call_line_of_a_stranger_states_that_the_chat_cannot_send() {
    use kourier_backend_api::IncomingMessage;
    use kourier_model::{Body, CannotSend, Origin, SystemEvent, UserId};
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let caller = UserId::from_parts("pn", "15550192736481").expect("a user");
    let line = BackendEvent::Message(IncomingMessage::new(
        ConvKey::Direct(caller.clone()),
        "CALL1",
        Origin::Remote {
            author: caller,
            device: None,
        },
        Body::System(SystemEvent::Joined),
        1_000,
        1_000,
    ));
    let writer = w.writer.clone();
    let events = vec![line.clone()];
    std::thread::spawn(move || writer.write_and_wait(&events))
        .join()
        .expect("handler thread")
        .expect("written");
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
    assert_eq!(rows.len(), 1, "one row");
    let features = match &rows[0][..] {
        [BackendEvent::ConversationUpsert(patch), message] if *message == line => {
            patch.features.clone()
        }
        _ => None,
    };
    assert_eq!(
        features.map(|f| f.cannot_send),
        Some(Some(CannotSend::NotAccepted)),
        "the statement before the call line says the chat cannot send"
    );
    w.owner.close();
}

/// Fix F9 (interop MEDIUM-1): the writer's zone moves the receipt of a message Kourier sent to the chat the ledger
/// holds for it. Kourier wrote the number chat to the peer's LID (D20), so the delivery receipt comes from the LID
/// and the core, which holds the chat under the number, would park it until the peer wrote. A message the ledger does
/// not know (one sent from the phone) keeps the chat the receipt came in.
#[tokio::test]
async fn a_receipt_from_the_lid_of_a_message_sent_into_the_number_chat_is_written_to_the_number_chat()
 {
    use crate::send::ledger::{Entry, Kind, Outcome, record};
    use crate::store::exec;
    use kourier_model::{MsgTarget, ReceiptKind, TargetAuthor, UserId};
    const PEER_PN: &str = "15550146392817";
    const PEER_LID: &str = "77700158204736";
    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let number = ConvKey::Direct(UserId::from_parts("pn", PEER_PN).expect("user"));
    let lid = ConvKey::Direct(UserId::from_parts("lid", PEER_LID).expect("user"));
    let sent = Entry {
        conv: number.clone(),
        local: "3EB0D1".into(),
        kind: Kind::Message,
        target: None,
        outcome: Outcome::Acked { server_t: Some(9) },
    };
    store
        .write("seed", move |tx| {
            exec(
                tx,
                "seed",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 0, 0, 'usync')",
                kourier_db::rusqlite::params![PEER_LID, PEER_PN],
            )?;
            record(tx, &sent, 1_000).map(|_| ())
        })
        .await
        .expect("seed");
    let receipt = |locals: &[&str]| BackendEvent::Receipts {
        targets: locals
            .iter()
            .map(|local| MsgTarget {
                conv: Some(lid.clone()),
                author: TargetAuthor::Own,
                local: (*local).into(),
                alternates: Vec::new(),
            })
            .collect(),
        by: UserId::from_parts("lid", PEER_LID).expect("user"),
        kind: ReceiptKind::Delivered,
        at: 2_000,
    };
    for events in [vec![receipt(&["3EB0D1"])], vec![receipt(&["3EB0FF"])]] {
        let writer = w.writer.clone();
        std::thread::spawn(move || writer.write_and_wait(&events))
            .join()
            .expect("handler thread")
            .expect("written");
    }
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
    let chats: Vec<Option<ConvKey>> = rows
        .iter()
        .map(|row| match &row[..] {
            [BackendEvent::Receipts { targets, by, .. }] => {
                let conv = targets[0].conv.clone();
                // The peer of the chat the receipt is for: the core applies a 1:1 receipt only from it.
                assert_eq!(
                    conv.as_ref().map(ConvKey::encode),
                    Some(ConvKey::Direct(by.clone()).encode()),
                    "the receipt's `by` is the peer of its chat"
                );
                conv
            }
            other => panic!("one receipt per row, got {} events", other.len()),
        })
        .collect();
    assert_eq!(
        chats,
        [Some(number), Some(lid)],
        "the ledger's message goes to the number chat, the phone's keeps the LID"
    );
    w.owner.close();
}

/// W6a2, B6: the server's `groups_dirty` writes a "fetch me" mark per group in one zone (the first ask's time stays), appends
/// no row to `core_outbox` (nothing is told to the core), and wakes the fetch policy after the commit. A write that marks
/// nothing wakes no one.
/// Mutants: the marks are written without waking the policy; the zone appends a row.
#[tokio::test]
async fn a_groups_dirty_marks_the_groups_and_wakes_the_fetch_policy_and_tells_the_core_nothing() {
    use crate::store::people;

    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let marks = w.writer.marks();
    let a = ConvKey::Group("wag:120363000071529611".into());
    let b = ConvKey::Group("wag:120363000071529622".into());

    // A write that marks nothing: no wake.
    let (thread, answer) = handler(&w.writer, 1);
    thread.join().expect("handler thread");
    assert_eq!(answer.recv().expect("answer"), Ok(()));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), marks.notified())
            .await
            .is_err(),
        "an ordinary write woke the fetch policy"
    );

    let writer = w.writer.clone();
    let named = vec![a.clone(), b.clone()];
    tokio::task::spawn_blocking(move || writer.write_groups_dirty_and_wait(named))
        .await
        .expect("join")
        .expect("written");
    tokio::time::timeout(Duration::from_secs(5), marks.notified())
        .await
        .expect("the fetch policy was not woken");
    assert_eq!(
        store.read(people::dirty).await.expect("marks"),
        vec![(a, 1_000_000), (b, 1_000_000)]
    );
    assert_eq!(
        written(&store).await,
        [1],
        "only the ordinary write's row: the marks tell the core nothing"
    );
    w.owner.close();
}

/// W6a2, B7a: the zone of a fetch's answer that states a group's members lets its waiting admin deletes go, and the writer
/// wakes stage 2 after that commit (the rows are due now); an answer that lets nothing go wakes nobody.
/// Mutant: the writer does not wake stage 2.
#[tokio::test]
async fn a_fetch_that_lets_waiting_deletes_go_wakes_stage_2() {
    use whatsapp_rust::{GroupMetadata, GroupMetadataResult};

    use crate::map::OwnIds;

    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let stage2 = Arc::new(Notify::new());
    w.writer.wakes_stage2_with(stage2.clone());
    let group = ConvKey::Group("wag:120363000071529611".into());
    let key = group.encode();
    store
        .write("test: a waiting delete", move |tx| {
            tx.execute(
                "INSERT INTO inbox(chat, sender, id, origin, received_at, catch_up, message, meta, state, parked_until, \
                                   members_wait) \
                 VALUES ('120363000071529611@g.us', 'x@lid', 'A', 'e2e', 1, 0, x'00', x'00', 'new', 4000000000000, ?1)",
                [key],
            )
            .map_err(sql("test: a waiting delete"))
            .map(drop)
        })
        .await
        .expect("insert");
    let answer = |result: GroupMetadataResult| {
        let writer = w.writer.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                writer.write_groups_fetched_and_wait(vec![result], OwnIds::default(), 1_000_000)
            })
            .await
            .expect("join")
            .expect("written");
        }
    };
    // An answer for another group lets nothing go.
    let mut other = GroupMetadata::new("120363000071529622@g.us".parse().expect("jid"));
    other.participants = vec![whatsapp_rust::GroupParticipant::new(
        "99000071522@lid".parse().expect("jid"),
    )];
    answer(GroupMetadataResult::Found(Box::new(other))).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), stage2.notified())
            .await
            .is_err(),
        "an answer that let nothing go woke stage 2"
    );
    // The answer for the waiting group does.
    let mut ours = GroupMetadata::new("120363000071529611@g.us".parse().expect("jid"));
    ours.participants = vec![whatsapp_rust::GroupParticipant::new(
        "99000071522@lid".parse().expect("jid"),
    )];
    answer(GroupMetadataResult::Found(Box::new(ours))).await;
    tokio::time::timeout(Duration::from_secs(5), stage2.notified())
        .await
        .expect("stage 2 was not woken");
    w.owner.close();
}

/// W6a2, B9: the merge sweep is woken by what can make a pair due, and only by that: a change of the phone's address book
/// and a group answer that states a member list. A plain event, an app-state mutation with no address book in it, an answer
/// that states nothing and the sweep's own write do not (the last would be a loop).
/// Mutants: no wake after an address-book change; no wake after a member list; a wake after every commit.
#[tokio::test]
async fn an_address_book_change_and_a_member_list_wake_the_merge_sweep_and_nothing_else_does() {
    use crate::appstate::map::{CONTACT, Mapped, Mirror};
    use crate::book::{Change, change_of_action};
    use crate::map::OwnIds;
    use whatsapp_rust::{GroupMetadata, GroupMetadataResult};

    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let merges = w.writer.merges();
    let woken = |why: &'static str| {
        let merges = merges.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(5), merges.notified())
                .await
                .unwrap_or_else(|_| panic!("the sweep was not woken: {why}"));
        }
    };
    let not_woken = |why: &'static str| {
        let merges = merges.clone();
        async move {
            assert!(
                tokio::time::timeout(Duration::from_millis(200), merges.notified())
                    .await
                    .is_err(),
                "the sweep was woken: {why}"
            );
        }
    };
    let run = |f: Box<dyn FnOnce(EventWriter) -> Result<(), WriteFailed> + Send>| {
        let writer = w.writer.clone();
        async move {
            tokio::task::spawn_blocking(move || f(writer))
                .await
                .expect("join")
                .expect("written");
        }
    };

    run(Box::new(|writer| writer.write_and_wait(&[event(1)]))).await;
    not_woken("a plain event").await;

    run(Box::new(|writer| {
        writer.write_settings_and_wait(Mapped {
            collection: CONTACT,
            mirror: None,
            events: Vec::new(),
            from_full_sync: false,
        })
    }))
    .await;
    not_woken("an app-state mutation that is no address book").await;

    let index: wacore_binary::Jid = "15550481401@s.whatsapp.net".parse().expect("a jid");
    let mut action = waproto::whatsapp::sync_action_value::ContactAction::default();
    action.full_name = Some("Ann Example".to_owned());
    let change: Change = change_of_action(&index, &action).expect("a change");
    run(Box::new(move |writer| {
        writer.write_settings_and_wait(Mapped {
            collection: CONTACT,
            mirror: Some(Mirror::Book(change)),
            events: Vec::new(),
            from_full_sync: false,
        })
    }))
    .await;
    woken("an address-book change").await;

    run(Box::new(|writer| writer.write_merges_and_wait(&[]))).await;
    not_woken("the sweep's own write").await;

    // A group that is no longer ours states nothing: no member list, no wake.
    let gone = GroupMetadataResult::NotFound("120363000071529633@g.us".parse().expect("jid"));
    run(Box::new(move |writer| {
        writer.write_groups_fetched_and_wait(vec![gone], OwnIds::default(), 1_000_000)
    }))
    .await;
    not_woken("an answer that states no member list").await;

    let mut ours = GroupMetadata::new("120363000071529644@g.us".parse().expect("jid"));
    ours.participants = vec![whatsapp_rust::GroupParticipant::new(
        "99000071522@lid".parse().expect("jid"),
    )];
    run(Box::new(move |writer| {
        writer.write_groups_fetched_and_wait(
            vec![GroupMetadataResult::Found(Box::new(ours))],
            OwnIds::default(),
            1_000_000,
        )
    }))
    .await;
    woken("a group's member list").await;
    w.owner.close();
}

/// F22 (DUR-13): the answer of a batch fetch is ONE zone and one `core_outbox` row PER GROUP, so the core ingests and refuses by
/// group: a single event it refuses quarantines that group's row, not the statements of every group the batch answered. The
/// zone is whole: a write that fails after the rows were made leaves no row and no group row (the next try writes all three).
/// Mutants: the groups' events appended as one row (in the zone, or in the writer).
#[tokio::test]
async fn a_batch_answer_is_one_zone_and_one_row_per_group() {
    use crate::map::OwnIds;
    use whatsapp_rust::{GroupMetadata, GroupMetadataResult};

    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let groups = [
        "120363000071529701",
        "120363000071529702",
        "120363000071529703",
    ];
    let answers = || -> Vec<GroupMetadataResult> {
        groups
            .iter()
            .map(|id| {
                let mut metadata = GroupMetadata::new(format!("{id}@g.us").parse().expect("jid"));
                metadata.participants = vec![whatsapp_rust::GroupParticipant::new(
                    "99000071522@lid".parse().expect("jid"),
                )];
                GroupMetadataResult::Found(Box::new(metadata))
            })
            .collect()
    };
    let write = |answers: Vec<GroupMetadataResult>| {
        let writer = w.writer.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                writer.write_groups_fetched_and_wait(answers, OwnIds::default(), 1_000_000)
            })
            .await
            .expect("join")
        }
    };
    // What the outbox and the groups' table hold: the groups each row names, and the number of group rows.
    let held = || {
        let store = store.clone();
        async move {
            store
                .read(|conn| {
                    let rows =
                        outbox::peek(conn, 100).map_err(|_| crate::error::database("peek"))?;
                    let named = rows
                        .iter()
                        .map(|row| {
                            let mut keys: Vec<String> = decode_outbox_row(&row.envelope)
                                .expect("decodes")
                                .into_iter()
                                .filter_map(|event| match event {
                                    BackendEvent::ConversationUpsert(patch) => {
                                        Some(patch.key.encode())
                                    }
                                    _ => None,
                                })
                                .collect();
                            keys.dedup();
                            keys
                        })
                        .collect::<Vec<_>>();
                    let states: i64 = conn
                        .query_row("SELECT count(*) FROM group_state", [], |r| r.get(0))
                        .map_err(sql("count the groups"))?;
                    Ok((named, states))
                })
                .await
                .expect("read")
        }
    };

    // A zone that fails after its rows were made leaves nothing.
    fault::arm(fault::EVENT_WRITE, 1);
    assert_eq!(write(answers()).await, Err(WriteFailed::Zone));
    assert_eq!(held().await, (Vec::<Vec<String>>::new(), 0));

    write(answers()).await.expect("written");
    let (named, states) = held().await;
    assert_eq!(states, 3, "a row for each group, in the same zone");
    assert_eq!(
        named,
        groups.map(|id| vec![format!("group:wag:{id}")]),
        "one row per group, each naming its own group only"
    );
    w.owner.close();
}

/// W6a2, B8b: the handler's `contact` mutation is one zone: the mirror always, and a name in `core_outbox` only for a person
/// the core knows (D8's scope rule), on a change only.
/// Mutants: the name is stated for a stranger; the mirror is not written for a stranger.
#[tokio::test]
async fn a_contact_mutation_names_only_a_person_the_core_knows() {
    use crate::appstate::map::{CONTACT, Mapped, Mirror};
    use crate::book::{Change, change_of_action};
    use crate::store::book;

    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let index: wacore_binary::Jid = "15550481301@s.whatsapp.net".parse().expect("a jid");
    let book_event = |name: &str| {
        let mut action = waproto::whatsapp::sync_action_value::ContactAction::default();
        action.full_name = Some(name.to_owned());
        let change: Change = change_of_action(&index, &action).expect("a change");
        Mapped {
            collection: CONTACT,
            mirror: Some(Mirror::Book(change)),
            events: Vec::new(),
            from_full_sync: false,
        }
    };
    let says = |mapped: Mapped| {
        let writer = w.writer.clone();
        async move {
            tokio::task::spawn_blocking(move || writer.write_settings_and_wait(mapped))
                .await
                .expect("join")
                .expect("written");
        }
    };
    let named = |store: KourierStore| async move {
        store
            .read(|conn| {
                let rows =
                    outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
                let mut names = Vec::new();
                for row in rows {
                    for event in decode_outbox_row(&row.envelope).expect("decodes") {
                        if let BackendEvent::ContactUpsert(patch) = event {
                            names.push(patch.system_name);
                        }
                    }
                }
                Ok(names)
            })
            .await
            .expect("read the outbox")
    };

    // A stranger: the mirror takes it, the core is told nothing.
    says(book_event("Ann Example")).await;
    assert!(
        named(store.clone()).await.is_empty(),
        "nothing for a stranger"
    );
    let kept = store
        .read(|conn| book::of(conn, "pn:15550481301"))
        .await
        .expect("read");
    assert!(kept.is_some(), "the mirror holds the phone's word");

    // Once the core knows him, the next mutation of the same contact is stated, a repeat is not.
    store
        .write("test: know", |tx| book::know(tx, "pn:15550481301"))
        .await
        .expect("know");
    says(book_event("Ann Renamed")).await;
    says(book_event("Ann Renamed")).await;
    assert_eq!(
        named(store.clone()).await,
        [kourier_model::Patch::Set("Ann Renamed".to_owned())],
        "one statement, on a change"
    );
    w.owner.close();
}

/// W6a2 PRIV-10, the other half of Q2 (D8's scope): a person only the phone's blocklist named is NOT known to the core, so the
/// address-book name the mirror holds for him is not stated. The blocklist's zone says `blocked` for him and nothing else
/// (no name, no number), and `persons_known` has no row for him; the name is stated where he first appears in a chat, a
/// group or a message.
/// Mutant: the blocklist's branch states the people it names like a chat does (`book::states`).
#[tokio::test]
async fn a_person_only_the_blocklist_named_is_not_known_and_gets_no_name() {
    use crate::store::book;
    use kourier_model::{Patch, UserId};

    let _serial = fault::serial();
    let w = world(super::MAX_GROUP);
    let store = w.owner.store();
    let stranger = UserId::from_parts("pn", "15550417358").expect("an id");
    let contact = stranger.as_str().to_owned();
    let kept = contact.clone();
    store
        .write("test: the phone's address book", move |tx| {
            book::put(
                tx,
                &book::Row {
                    contact: kept.clone(),
                    name: "Synthetic Blocked Book".to_owned(),
                    pn: Some(kept),
                    lid: None,
                },
            )
            .map(drop)
        })
        .await
        .expect("the mirror takes the name");
    let writer = w.writer.clone();
    let listed = [stranger.clone()];
    tokio::task::spawn_blocking(move || writer.write_blocklist_and_wait(&listed))
        .await
        .expect("join")
        .expect("written");
    let told = store
        .read(|conn| {
            let rows = outbox::peek(conn, 10_000).map_err(|_| crate::error::database("peek"))?;
            let mut told = Vec::new();
            for row in rows {
                for event in decode_outbox_row(&row.envelope).expect("decodes") {
                    if let BackendEvent::ContactUpsert(patch) = event {
                        told.push((patch.user, patch.blocked, patch.system_name));
                    }
                }
            }
            Ok(told)
        })
        .await
        .expect("read the outbox");
    assert_eq!(
        told,
        [(stranger, Some(true), Patch::Unchanged)],
        "blocked, and no name of the mirror"
    );
    let known = store
        .read(move |conn| book::known_of(conn, &[contact.as_str()]))
        .await
        .expect("read");
    assert!(known.is_empty(), "the blocklist made him known: {known:?}");
    w.owner.close();
}
