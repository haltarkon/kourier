//! Settling and the bounds of the delivery (W4 C14): the window of two unsettled rows, a row the core refuses
//! (cut into singles, a refused single dropped and counted), a row the core never answers (the idle bound),
//! a row found unanswered at too many starts, and a delivery that cannot find its place. The core is a few
//! lines here (`serve`): it takes the rows of `core_outbox` in order and acks, refuses for good, or sticks.
//! Every id, number and secret is synthetic.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, FakeClock, decode_outbox_row, encode_outbox_row};
use kourier_db::{DbKey, outbox};
use kourier_model::UserId;
use wacore::history_sync::MAX_DECOMPRESSED;
use wacore::store::error::StoreError;
use wacore_binary::Jid;

use super::{Pump, Rows, Step};
use crate::error::sql;
use crate::history::deliver::{Own, Source};
use crate::history::feed::{Entry, scan};
use crate::history::rows::{Bounds, Cursor, Next, Phase, Reader, Row};
use crate::history::settle::Limits;
use crate::history::testing::{Chat, Chunk, Msg, SyncType};
use crate::map::{Ctx, OwnIds};
use crate::store::{KourierStore, StoreOwner};

const P1: &str = "15550100101@s.whatsapp.net";
const P2: &str = "15550100102@s.whatsapp.net";
const P3: &str = "15550100103@s.whatsapp.net";
const OWN_PN: &str = "15550100999";
const OWN_LID: &str = "99900100999";
const NOW: u64 = 1_700_000_500_000;
const SEQ: i64 = 1;

struct World {
    _dir: tempfile::TempDir,
    _owner: StoreOwner,
    store: KourierStore,
    clock: FakeClock,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([9; 32]),
        "wa-pump",
    )
    .expect("open");
    let store = owner.store();
    World {
        _dir: dir,
        _owner: owner,
        store,
        clock: FakeClock::new(NOW),
    }
}

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(UserId::from_parts("lid", OWN_LID).expect("lid")),
            pn: Some(UserId::from_parts("pn", OWN_PN).expect("pn")),
        },
    }
}

/// One planned chunk as the feeder of the delivery: the rows of chunk [`SEQ`].
struct Feed {
    bytes: Vec<u8>,
    entries: Vec<Entry>,
    ctx: Ctx,
}

impl Feed {
    fn of(chunk: &Chunk) -> Self {
        let bytes = chunk.blob();
        let entries = scan(&bytes, MAX_DECOMPRESSED).expect("scan").entries;
        Self {
            bytes,
            entries,
            ctx: ctx(),
        }
    }

    fn read(&self, at: Cursor, bounds: Bounds) -> Option<(Source, Row)> {
        let mut reader = Reader::new(&self.bytes, &self.entries, &self.ctx, NOW as i64);
        match reader.next(at, bounds).expect("a row") {
            Next::Row(row) => Some((source(), row)),
            Next::End => None,
        }
    }
}

fn source() -> Source {
    Source {
        msg_id: "n1".to_owned(),
        sync_type: Some(0),
        progress: Some(0.4),
    }
}

impl Rows for Feed {
    async fn next(&mut self, cursor: Cursor) -> Result<Option<(Source, Row)>, StoreError> {
        let at = if cursor.chunk == 0 {
            Cursor::start(SEQ)
        } else if cursor.chunk == SEQ {
            cursor
        } else {
            return Ok(None);
        };
        Ok(self.read(at, Bounds::DEFAULT))
    }

    async fn single(&mut self, at: Cursor) -> Result<Option<(Source, Row)>, StoreError> {
        Ok(self.read(at, Bounds::SINGLE))
    }
}

/// A feeder that always offers the first row of the chunk, whatever the cursor says.
struct Stale(Feed);

impl Rows for Stale {
    async fn next(&mut self, _cursor: Cursor) -> Result<Option<(Source, Row)>, StoreError> {
        Ok(self.0.read(Cursor::start(SEQ), Bounds::DEFAULT))
    }

    async fn single(&mut self, at: Cursor) -> Result<Option<(Source, Row)>, StoreError> {
        Ok(self.0.read(at, Bounds::SINGLE))
    }
}

fn own() -> Own {
    Own {
        pn: Some(Jid::pn(OWN_PN)),
        lid: Some(Jid::lid(OWN_LID)),
    }
}

async fn start_pump<R: Rows>(w: &World, rows: R, limits: Limits) -> Pump<R> {
    Pump::start(
        w.store.clone(),
        rows,
        own(),
        Arc::new(w.clock.clone()),
        limits,
    )
    .await
    .expect("the pump starts")
}

/// A chat of the given messages ("A1".. in the order given, a minute apart), unread count and last activity.
fn chat(jid: &str, unread: u32, active: u64, ids: &[&str]) -> Chat {
    ids.iter().zip(0u64..).fold(
        Chat::new(jid).unread(unread).active_at(active),
        |chat, (id, n)| chat.message(Msg::text(jid, id, 1_700_000_100 + 60 * n, id)),
    )
}

// ---- the core, in a few lines ------------------------------------------------------------------------

/// What the core does with a row.
enum Verdict {
    Ack,
    /// Refused for good: the forwarder quarantines the row and goes on.
    Refuse,
    /// A retryable refusal for ever: the forwarder ships the row again and nothing behind it moves.
    Stick,
}

/// What the core was given, in order: the locals of the messages and `LIVE` for a live row.
#[derive(Default)]
struct Core {
    acked: Vec<String>,
    hints: Vec<(String, u32)>,
}

const LIVE: &str = "LIVE";

fn locals(events: &[BackendEvent]) -> Vec<String> {
    events
        .iter()
        .flat_map(|event| match event {
            BackendEvent::HistoryBatch { messages, .. } => messages
                .iter()
                .map(|m| m.local.to_string())
                .collect::<Vec<_>>(),
            BackendEvent::PinnedConversations(_) => vec![LIVE.to_owned()],
            _ => Vec::new(),
        })
        .collect()
}

/// The core goes through `core_outbox` in order; `judge` says what it does with each row.
async fn serve(w: &World, core: &mut Core, mut judge: impl FnMut(&[BackendEvent]) -> Verdict) {
    let rows = w
        .store
        .read(|conn| outbox::peek(conn, 1000).map_err(|_| crate::error::database("peek")))
        .await
        .expect("peek");
    for row in rows {
        let events = decode_outbox_row(&row.envelope).expect("decodes");
        let verdict = judge(&events);
        let seq = row.seq;
        match verdict {
            Verdict::Stick => break,
            Verdict::Refuse => {
                w.store
                    .write("test core", move |tx| {
                        outbox::quarantine(tx, seq, "refused", NOW as i64)
                            .map(drop)
                            .map_err(|_| crate::error::database("quarantine"))
                    })
                    .await
                    .expect("quarantine");
            }
            Verdict::Ack => {
                w.store
                    .write("test core", move |tx| {
                        tx.execute("DELETE FROM core_outbox WHERE seq = ?1", [seq])
                            .map(drop)
                            .map_err(sql("ack"))
                    })
                    .await
                    .expect("ack");
                core.acked.extend(locals(&events));
                for event in &events {
                    if let BackendEvent::ConversationUpsert(patch) = event
                        && let Some(hint) = patch.unread_hint
                    {
                        core.hints.push((patch.key.encode(), hint));
                    }
                }
            }
        }
    }
}

async fn ack_all(w: &World, core: &mut Core) {
    serve(w, core, |_| Verdict::Ack).await;
}

/// A row of the live path, behind whatever is in `core_outbox`.
async fn live_row(w: &World) {
    let bytes =
        encode_outbox_row(&[BackendEvent::PinnedConversations(Vec::new())]).expect("encode");
    w.store
        .write("test live", move |tx| {
            outbox::append(tx, &bytes, NOW as i64)
                .map(drop)
                .map_err(|_| crate::error::database("append"))
        })
        .await
        .expect("live row");
}

// ---- reading the books -------------------------------------------------------------------------------

async fn count(w: &World, sql_text: &'static str) -> i64 {
    w.store
        .read(move |conn| {
            conn.query_row(&format!("SELECT count(*) FROM {sql_text}"), [], |r| {
                r.get(0)
            })
            .map_err(sql("count"))
        })
        .await
        .expect("count")
}

/// The `seq` of every row of `core_outbox`, in order.
async fn outbox_seqs(w: &World) -> Vec<i64> {
    w.store
        .read(|conn| {
            outbox::peek(conn, 1000)
                .map(|rows| rows.into_iter().map(|row| row.seq).collect())
                .map_err(|_| crate::error::database("peek"))
        })
        .await
        .expect("peek")
}

async fn counts(w: &World) -> (i64, i64) {
    w.store
        .read(|conn| {
            conn.query_row(
                "SELECT delivered, dropped FROM history_counts WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sql("counts"))
        })
        .await
        .expect("counts")
}

async fn cursor_phase(w: &World) -> i64 {
    w.store
        .read(|conn| {
            conn.query_row("SELECT phase FROM history_cursor WHERE id = 1", [], |r| {
                r.get(0)
            })
            .map_err(sql("cursor"))
        })
        .await
        .expect("cursor")
}

/// Steps until the pump has nothing to do at once; returns what it ended on. At most `bound` steps.
async fn settle_down<R: Rows>(w: &World, pump: &mut Pump<R>, bound: usize) -> Step {
    for _ in 0..bound {
        let step = pump.step().await.expect("a step");
        let waiting = count(w, "history_delivery WHERE seq IS NOT NULL").await;
        assert!(
            waiting <= i64::try_from(pump.limits.window).unwrap_or(i64::MAX),
            "{waiting} rows unsettled"
        );
        if !matches!(step, Step::Progress { .. }) {
            return step;
        }
    }
    panic!("the pump did not come to rest in {bound} steps");
}

// ---- the tests ---------------------------------------------------------------------------------------

#[tokio::test]
async fn at_most_two_rows_unsettled() {
    let w = world();
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(chat(P1, 0, 1_700_000_400, &["A1", "A2"]))
        .chat(chat(P2, 0, 1_700_000_300, &["B1", "B2"]))
        .chat(chat(P3, 0, 1_700_000_200, &["C1", "C2"]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();

    // A chats row and the messages of three chats: four rows, and the core answers nothing.
    let step = settle_down(&w, &mut pump, 20).await;
    assert!(matches!(step, Step::Wait { .. }), "{step:?}");
    assert_eq!(count(&w, "core_outbox").await, 2, "exactly the window");
    assert_eq!(count(&w, "history_delivery").await, 2);

    // The core answers: two more follow, never more than two at a time (`settle_down` checks each step).
    for _ in 0..4 {
        ack_all(&w, &mut core).await;
        settle_down(&w, &mut pump, 20).await;
    }
    ack_all(&w, &mut core).await;
    let end = settle_down(&w, &mut pump, 20).await;
    assert_eq!(end, Step::Quiet, "nothing waits and nothing is left");
    assert_eq!(count(&w, "core_outbox").await, 0);
    assert_eq!(count(&w, "history_delivery").await, 0);
    assert_eq!(cursor_phase(&w).await, Phase::End.number());
    assert_eq!(core.acked.len(), 6, "{:?}", core.acked);
    assert_eq!(counts(&w).await, (6, 0));
}

#[tokio::test]
async fn a_refused_row_becomes_singles_and_a_refused_single_is_dropped_and_counted() {
    let w = world();
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40).chat(chat(
        P1,
        0,
        1_700_000_400,
        &["A1", "A2", "A3"],
    ));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let refuse = |events: &[BackendEvent]| {
        // The core cannot take the message A2, alone or in a row.
        if locals(events).iter().any(|local| local == "A2") {
            Verdict::Refuse
        } else {
            Verdict::Ack
        }
    };

    for _ in 0..40 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, refuse).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert_eq!(
        pump.step().await.expect("a step"),
        Step::Quiet,
        "it came to rest"
    );
    // The row of three was refused and cut; the refused one alone was dropped; the other two went once.
    assert_eq!(
        core.acked,
        ["A1", "A3"],
        "each message the core can take, once"
    );
    assert_eq!(counts(&w).await, (2, 1), "delivered, dropped");
    assert_eq!(count(&w, "core_outbox").await, 0);
    assert_eq!(
        count(&w, "core_outbox_poison").await,
        0,
        "a refused history is not kept"
    );
    assert_eq!(count(&w, "history_delivery").await, 0);
    assert_eq!(cursor_phase(&w).await, Phase::End.number());
}

#[tokio::test]
async fn a_live_row_behind_a_history_single_the_core_never_acks_arrives_after_the_idle_bound() {
    let w = world();
    let chunk =
        Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40).chat(chat(P1, 0, 1_700_000_400, &["A1"]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    // The core sticks on the message A1 for ever (a retryable refusal): nothing behind it moves.
    let stick = |events: &[BackendEvent]| {
        if locals(events).iter().any(|local| local == "A1") {
            Verdict::Stick
        } else {
            Verdict::Ack
        }
    };

    settle_down(&w, &mut pump, 10).await;
    live_row(&w).await;
    serve(&w, &mut core, stick).await;
    assert!(
        core.acked.is_empty(),
        "the chats row is acked without a message: {:?}",
        core.acked
    );
    let mut elapsed = Duration::ZERO;
    let mut arrived = None;
    for _ in 0..60 {
        w.clock.advance(Duration::from_secs(30));
        elapsed += Duration::from_secs(30);
        pump.step().await.expect("a step");
        serve(&w, &mut core, stick).await;
        if core.acked.iter().any(|local| local == LIVE) {
            arrived = Some(elapsed);
            break;
        }
    }
    let arrived = arrived.expect("the live row arrived");
    assert!(
        arrived >= Duration::from_secs(9 * 60),
        "not before the bound: after {arrived:?}"
    );
    assert!(arrived <= Duration::from_secs(12 * 60), "after {arrived:?}");
    assert_eq!(
        counts(&w).await,
        (0, 1),
        "the stuck message was dropped and counted"
    );
    assert!(!core.acked.iter().any(|local| local == "A1"));
}

#[tokio::test]
async fn a_row_unacked_at_three_starts_is_split() {
    let w = world();
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40).chat(chat(
        P1,
        0,
        1_700_000_400,
        &["A1", "A2", "A3"],
    ));
    let mut core = Core::default();
    // The core takes the chats row and sticks on the row of messages.
    let stick = |events: &[BackendEvent]| {
        if locals(events)
            .iter()
            .any(|local| local == "A1" || local == "A2")
        {
            Verdict::Stick
        } else {
            Verdict::Ack
        }
    };
    let mut first = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    settle_down(&w, &mut first, 10).await;
    serve(&w, &mut core, stick).await;
    // The chats row was answered: a look settles it.
    first.step().await.expect("a step");
    drop(first);
    assert_eq!(
        count(&w, "core_outbox").await,
        1,
        "the row of messages is not answered"
    );

    // Starts 2, 3 and 4 find it unanswered: counted, not yet taken back.
    for start in 2..=4 {
        let again = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
        drop(again);
        assert_eq!(count(&w, "core_outbox").await, 1, "start {start}");
        assert_eq!(count(&w, "history_delivery WHERE seq IS NOT NULL").await, 1);
    }
    // The fifth finds it at more than three starts: it is cut.
    let mut last = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    assert_eq!(count(&w, "core_outbox").await, 0, "taken back");
    assert_eq!(
        count(&w, "history_delivery WHERE seq IS NULL").await,
        1,
        "waits to be redone"
    );
    assert_eq!(
        counts(&w).await,
        (0, 0),
        "its messages are not counted as delivered"
    );

    // Redone one unit at a time, the core takes them now.
    for _ in 0..40 {
        let step = last.step().await.expect("a step");
        ack_all(&w, &mut core).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert_eq!(
        core.acked,
        ["A1", "A2", "A3"],
        "every message once, in order"
    );
    assert_eq!(counts(&w).await, (3, 0));
    assert_eq!(count(&w, "history_delivery").await, 0);
}

#[tokio::test]
async fn a_live_row_ahead_of_the_history_does_not_age_it() {
    let w = world();
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40).chat(chat(
        P1,
        0,
        1_700_000_400,
        &["A1", "A2"],
    ));
    let mut core = Core::default();
    // A row of the live path is first in `core_outbox` and the core sticks on IT.
    live_row(&w).await;
    let stick_live = |events: &[BackendEvent]| {
        if locals(events).iter().any(|local| local == LIVE) {
            Verdict::Stick
        } else {
            Verdict::Ack
        }
    };
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    settle_down(&w, &mut pump, 10).await;
    assert_eq!(count(&w, "history_delivery").await, 2);
    let seqs = outbox_seqs(&w).await;
    assert_eq!(seqs.len(), 3, "the live row and the two history rows");

    // Far past the idle bound: the history rows are not at fault, nothing is taken back.
    for _ in 0..40 {
        w.clock.advance(Duration::from_secs(30));
        pump.step().await.expect("a step");
        serve(&w, &mut core, stick_live).await;
    }
    assert_eq!(count(&w, "history_delivery WHERE seq IS NOT NULL").await, 2);
    assert_eq!(
        outbox_seqs(&w).await,
        seqs,
        "no row was taken back and appended again (a split row would have new seqs)"
    );
    assert_eq!(
        counts(&w).await,
        (2, 0),
        "nothing dropped, nothing given back"
    );

    // The core gets over it and everything goes through once.
    for _ in 0..20 {
        let step = pump.step().await.expect("a step");
        ack_all(&w, &mut core).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert_eq!(core.acked, [LIVE, "A1", "A2"]);
}

#[tokio::test]
async fn a_taken_back_chats_row_states_its_hint_again() {
    let w = world();
    // Two chats with unread counts: one chats row of two units.
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(chat(P1, 3, 1_700_000_400, &["A1"]))
        .chat(chat(P2, 2, 1_700_000_300, &["B1"]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let mut refused = false;
    // The core refuses the first row that carries the chats (once), and takes everything else.
    let mut judge = move |events: &[BackendEvent]| {
        let chats = events
            .iter()
            .any(|event| matches!(event, BackendEvent::ConversationUpsert(_)));
        if chats && !refused {
            refused = true;
            Verdict::Refuse
        } else {
            Verdict::Ack
        }
    };
    for _ in 0..60 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, &mut judge).await;
        if step == Step::Quiet {
            break;
        }
    }
    let mut stated = core.hints.clone();
    stated.sort();
    assert_eq!(
        stated.len(),
        2,
        "each chat's hint reached the core once: {stated:?}"
    );
    let mut hints: Vec<u32> = stated.iter().map(|(_, hint)| *hint).collect();
    hints.sort_unstable();
    assert_eq!(hints, [2, 3]);
    assert_eq!(count(&w, "history_chats WHERE hint_stated = 1").await, 2);
    assert_eq!(core.acked, ["A1", "B1"]);
}

/// W5a, D9: a chats row the core refused carried the features of its chats; they are stated again with the redo,
/// so each chat's features reach the core.
#[tokio::test]
async fn a_taken_back_chats_row_states_its_chats_features_again() {
    let w = world();
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(chat(P1, 3, 1_700_000_400, &["A1"]))
        .chat(chat(P2, 2, 1_700_000_300, &["B1"]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let mut refused = false;
    let stated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = stated.clone();
    let mut judge = move |events: &[BackendEvent]| {
        let chats = events
            .iter()
            .any(|event| matches!(event, BackendEvent::ConversationUpsert(_)));
        if chats && !refused {
            refused = true;
            return Verdict::Refuse;
        }
        for event in events {
            if let BackendEvent::ConversationUpsert(patch) = event
                && crate::features::is_statement(event)
            {
                seen.lock().expect("lock").push(patch.key.encode());
            }
        }
        Verdict::Ack
    };
    for _ in 0..60 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, &mut judge).await;
        if step == Step::Quiet {
            break;
        }
    }
    let mut stated = stated.lock().expect("lock").clone();
    stated.sort();
    stated.dedup();
    assert_eq!(stated.len(), 2, "each chat's features reached the core");
    assert_eq!(core.acked, ["A1", "B1"]);
}

/// W6a1 (H5): the settings the phone's app state gave a chat the core did not know ride in the row that first states the
/// chat (the delivery's zone is one of those); when the core refuses that row the settings are due again with the redo,
/// or the chat would stay without them for good.
/// Mutant: `forget_hints_of_row` leaves `chat_state.stated` alone.
#[tokio::test]
async fn a_taken_back_chats_row_states_the_phones_settings_of_its_chat_again() {
    use crate::store::chat_state::{Field, Put, put};
    let w = world();
    let chat_key = kourier_model::ConvKey::Direct(
        kourier_model::UserId::from_parts("pn", "15550100101").expect("a user"),
    );
    {
        let chat_key = chat_key.clone();
        w.store
            .write("the phone's word", move |tx| {
                put(
                    tx,
                    &Put {
                        chat: &chat_key,
                        field: Field::Archived,
                        value: 1,
                        action_ms: 10,
                        generation: 0,
                        collection: "regular_low",
                        stated: false,
                    },
                )
                .map(drop)
            })
            .await
            .expect("mirror");
    }
    // Two chats: a refused row of one chat is dropped, a refused row of several is split and redone.
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(chat(P1, 3, 1_700_000_400, &["A1"]))
        .chat(chat(P2, 2, 1_700_000_300, &["B1"]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let mut refused = false;
    let told = Arc::new(std::sync::Mutex::new(0usize));
    let seen = told.clone();
    let mut judge = move |events: &[BackendEvent]| {
        let chats = events
            .iter()
            .any(|event| matches!(event, BackendEvent::ConversationUpsert(_)));
        if chats && !refused {
            refused = true;
            return Verdict::Refuse;
        }
        for event in events {
            if matches!(event, BackendEvent::ConversationUpsert(p)
                if p.key == chat_key && p.archived == Some(true))
            {
                *seen.lock().expect("lock") += 1;
            }
        }
        Verdict::Ack
    };
    for _ in 0..60 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, &mut judge).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert_eq!(
        *told.lock().expect("lock"),
        1,
        "the redo tells the core the chat is archived"
    );
}

// ---- W6a2 F18 (DUR-4): a refused row's statements about people are taken back with it ----

const ANN_PN_ID: &str = "pn:15550100101";
const ANN_LID_ID: &str = "lid:99900100101";
const ANN_BOOK_NAME: &str = "Ann Bookname";
const ANN_PUSH_NAME: &str = "Ann Pushname";

/// The phone's address book names Ann, and an earlier history chunk kept her pair and her push name while she was a stranger:
/// the zone that first states her chat owes the core all three.
async fn ann_is_owed(w: &World) {
    w.store
        .write("test: what Ann is owed", |tx| {
            crate::store::book::put(
                tx,
                &crate::store::book::Row {
                    contact: ANN_PN_ID.to_owned(),
                    name: ANN_BOOK_NAME.to_owned(),
                    pn: Some(ANN_PN_ID.to_owned()),
                    lid: None,
                },
            )?;
            crate::store::strangers::keep_pair(tx, ANN_PN_ID, ANN_LID_ID)?;
            crate::store::strangers::keep_name(tx, ANN_PN_ID, ANN_PUSH_NAME)
        })
        .await
        .expect("seed");
}

/// What the core was told of Ann in the rows it took: `(pairs, address-book names, push names)`.
fn told_of_ann(events: &[BackendEvent]) -> (usize, usize, usize) {
    use kourier_model::Patch;
    let mut told = (0, 0, 0);
    for event in events {
        match event {
            BackendEvent::IdentityMerged { from, .. } if from.as_str() == ANN_PN_ID => told.0 += 1,
            BackendEvent::ContactUpsert(patch) if patch.user.as_str() == ANN_PN_ID => {
                if patch.system_name == Patch::Set(ANN_BOOK_NAME.to_owned()) {
                    told.1 += 1;
                }
                if patch.profile_name == Patch::Set(ANN_PUSH_NAME.to_owned()) {
                    told.2 += 1;
                }
            }
            _ => {}
        }
    }
    told
}

/// DUR-4: the first chats row carried Ann's pair, her address-book name and her push name; the core refuses it. The take-back
/// forgets that the core was told (`merges_stated`, `names_stated`) and keeps the pair and the push name again, so the redo
/// of her chat (a row of two chats is split) carries all three, once.
/// Mutants: the `merges_stated` row left; the `names_stated` system and number rows left; the push name not kept again; the
/// pair not kept again.
#[tokio::test]
async fn a_taken_back_chats_row_states_the_people_it_named_again() {
    let w = world();
    ann_is_owed(&w).await;
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(chat(P1, 3, 1_700_000_400, &["A1"]))
        .chat(chat(P2, 2, 1_700_000_300, &["B1"]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let mut refused = false;
    let told = Arc::new(std::sync::Mutex::new((0usize, 0usize, 0usize)));
    let seen = told.clone();
    let mut judge = move |events: &[BackendEvent]| {
        let chats = events
            .iter()
            .any(|event| matches!(event, BackendEvent::ConversationUpsert(_)));
        if chats && !refused {
            assert_eq!(
                told_of_ann(events),
                (1, 1, 1),
                "the row that is refused carries all three"
            );
            refused = true;
            return Verdict::Refuse;
        }
        let (a, b, c) = told_of_ann(events);
        let mut seen = seen.lock().expect("lock");
        seen.0 += a;
        seen.1 += b;
        seen.2 += c;
        Verdict::Ack
    };
    for _ in 0..60 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, &mut judge).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert_eq!(
        *told.lock().expect("lock"),
        (1, 1, 1),
        "the redo tells the core her pair, her address-book name and her push name, once"
    );
    assert_eq!(core.acked, ["A1", "B1"]);
    assert_eq!(count(&w, "kept_pairs").await, 0, "released again");
    assert_eq!(count(&w, "kept_names").await, 0, "released again");
}

/// DUR-4: a chats row of ONE chat that the core refuses is dropped; the person it made known is not known to the core, so
/// the statements are taken back and `persons_known` forgets him (a later address-book change is not stated for someone the
/// core never got); the pair and the name wait in the kept tables for the day he is known.
/// Mutant: the dropped row's people stay in `persons_known`.
#[tokio::test]
async fn a_dropped_chats_row_makes_nobody_known_to_the_core() {
    let w = world();
    ann_is_owed(&w).await;
    let chunk =
        Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40).chat(chat(P1, 0, 1_700_000_400, &[]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let mut refused = 0;
    let mut judge = |events: &[BackendEvent]| {
        if events
            .iter()
            .any(|event| matches!(event, BackendEvent::ConversationUpsert(_)))
        {
            assert_eq!(told_of_ann(events), (1, 1, 1));
            refused += 1;
            Verdict::Refuse
        } else {
            Verdict::Ack
        }
    };
    for _ in 0..60 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, &mut judge).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert_eq!(refused, 1, "the chats row was refused and dropped");
    assert_eq!(count(&w, "persons_known").await, 0, "nobody is known");
    assert_eq!(count(&w, "merges_stated").await, 0);
    assert_eq!(count(&w, "names_stated").await, 0);
    assert_eq!(
        count(&w, "kept_pairs").await,
        1,
        "kept for the day she is known"
    );
    assert_eq!(count(&w, "kept_names").await, 1);
    assert_eq!(count(&w, "address_book").await, 1, "the phone's word stays");
}

/// W6a2 round 2, G2 (NEW-1): a pair the core was told by an EARLIER zone (an acked row, the live path, the sweep) stays told
/// when a refused chats row only REPEATS it (the chat names both of the person's ids): `merges::record` is idempotent, so the
/// refused row recorded nothing, and its take-back forgets a pair only for the row that recorded it. Without that the crossing
/// guard around the number was gone and the sweep could state a second pair around it (a recycled number, N27).
/// Mutant: `forget_told_by` without its `by_seq` condition.
#[tokio::test]
async fn a_refused_row_that_repeats_a_told_pair_leaves_the_record() {
    let w = world();
    w.store
        .write("test: an earlier zone told the pair", |tx| {
            crate::store::merges::put_stated(tx, ANN_PN_ID, ANN_LID_ID).map(drop)
        })
        .await
        .expect("told");
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(chat(P1, 0, 1_700_000_400, &[]).lid("99900100101@lid"));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let (mut refused, mut repeated) = (0, 0);
    let mut judge = |events: &[BackendEvent]| {
        if events
            .iter()
            .any(|event| matches!(event, BackendEvent::ConversationUpsert(_)))
        {
            repeated += events
                .iter()
                .filter(|event| {
                    matches!(event, BackendEvent::IdentityMerged { from, into }
                        if from.as_str() == ANN_PN_ID && into.as_str() == ANN_LID_ID)
                })
                .count();
            refused += 1;
            Verdict::Refuse
        } else {
            Verdict::Ack
        }
    };
    for _ in 0..60 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, &mut judge).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert_eq!(refused, 1, "the chats row was refused and dropped");
    assert_eq!(repeated, 1, "the refused row repeated the pair");
    assert_eq!(
        count(&w, "merges_stated").await,
        1,
        "the pair the earlier zone told is still recorded as told"
    );
}

/// G2 (NEW-2): the unarchive that a history row's zone puts in front of its batch (the phone's "keep chats archived" OFF
/// moved the chat out: `appstate::zone::after_history`) clears the mirror's archive in that zone. When the core refuses
/// the row it never heard the word and still holds the chat archived, so the take-back puts the mirror's archive back
/// and the redo's row says `archived: false` again.
/// Mutants: `restore_archive` changes nothing; the take-back leaves a messages row alone.
#[tokio::test]
async fn a_taken_back_messages_row_states_the_phones_unarchive_again() {
    use crate::store::chat_state::{self, Field, Put, put};
    let w = world();
    let chat_key = kourier_model::ConvKey::Direct(
        kourier_model::UserId::from_parts("pn", "15550100101").expect("a user"),
    );
    {
        let chat_key = chat_key.clone();
        w.store
            .write("the phone's word", move |tx| {
                // The core knows the chat (its features were stated), has it archived, and the phone's setting says a
                // new message brings a chat back.
                crate::send::started::mark_stated(tx, &chat_key, crate::features::VERSION, true)?;
                chat_state::set_account_flag(tx, chat_state::UNARCHIVE_CHATS, true)?;
                put(
                    tx,
                    &Put {
                        chat: &chat_key,
                        field: Field::Archived,
                        value: 1,
                        action_ms: 10,
                        generation: 0,
                        collection: "regular_low",
                        stated: true,
                    },
                )
                .map(drop)
            })
            .await
            .expect("mirror");
    }
    // Three messages of the chat newer than the archive: a refused row of several units is split and redone.
    let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .chat(chat(P1, 3, 1_700_000_400, &["A1", "A2", "A3"]))
        .chat(chat(P2, 2, 1_700_000_300, &["B1"]));
    let mut pump = start_pump(&w, Feed::of(&chunk), Limits::DEFAULT).await;
    let mut core = Core::default();
    let (mut refused, mut told) = (false, 0usize);
    let mut judge = |events: &[BackendEvent]| {
        let unarchives = events.iter().any(|event| {
            matches!(event, BackendEvent::ConversationUpsert(p)
                if p.key == chat_key && p.archived == Some(false))
        });
        if unarchives && !refused {
            refused = true;
            return Verdict::Refuse;
        }
        if unarchives {
            told += 1;
        }
        Verdict::Ack
    };
    for _ in 0..60 {
        let step = pump.step().await.expect("a step");
        serve(&w, &mut core, &mut judge).await;
        if step == Step::Quiet {
            break;
        }
    }
    assert!(
        refused,
        "the first row that carried the unarchive was refused"
    );
    assert_eq!(
        told, 1,
        "the redo tells the core the chat is out of the archive, once"
    );
    let key = chat_key.clone();
    let row = w
        .store
        .read(move |conn| chat_state::get(conn, &key, Field::Archived))
        .await
        .expect("get")
        .expect("a row");
    assert_eq!(row.value, 0, "and the mirror follows the core again");
}

#[tokio::test]
async fn a_delivery_that_cannot_find_its_place_gives_up() {
    let w = world();
    let chunk =
        Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40).chat(chat(P1, 0, 1_700_000_400, &["A1"]));
    let mut pump = start_pump(&w, Stale(Feed::of(&chunk)), Limits::DEFAULT).await;
    // The first row is right; the feeder then offers the same first row again and again.
    assert!(matches!(
        pump.step().await,
        Ok(Step::Progress { seq: Some(_) })
    ));
    for n in 1..=3 {
        assert!(
            matches!(pump.step().await, Ok(Step::Progress { seq: None })),
            "out of step {n}: read again"
        );
    }
    assert!(
        pump.step().await.is_err(),
        "the fourth in a row ends the delivery"
    );
}
