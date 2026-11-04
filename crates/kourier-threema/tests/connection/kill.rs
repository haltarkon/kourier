//! TH5a A9a: THE KILL LOOPS OF THE FOLLOWER RECEIVE through a REAL actor (brief §2.3 "THE KILL WINDOWS", §2.12;
//! critic M4). The phone in the PARENT seals a mixed backlog of 36 envelopes (a contact sync, a group sync with a
//! notice, a poll vote (deferred), an edit (an `Edited` event), a text of an unknown sender (dropped), 1:1, group and outgoing texts, two
//! replays of earlier texts) that the mediator hands the NEW slot right after its first `ServerInfo` (more than one
//! batch zone of 30). The actor runs in an `unshare -rn` CHILD over the parent-resident fake (`support/piped.rs`); its
//! core never acks, so `core_outbox` + `core_outbox_poison` keep every row the children wrote.
//!
//! THE WINDOWS: K1 `threema_inside_reflected:n` (killed inside the n-th zone), K2 `threema_after_reflected:n`
//! (committed, no ack sent), each for n = 1, 2, ... on a FRESH world until the child passes (n past the last zone);
//! K3: `threema_after_reflected_ack:k` (the child dies right after its k-th ack went out), so the kill
//! lands on the send path, after some acks of a zone. After EACH kill the parent reads the file and the fake: the acks
//! are a prefix of the slot's queue, the queue holds the rest, and the file is EXACTLY the state of a prefix of the
//! backlog (K1: the acked prefix; K2: a longer one; K3: at least the acked one), its rows the reference's first rows.
//! Then a SECOND child on the same world runs to `Syncing` (the login-ack comes after the backlog and Dry) and ends.
//! At the end: every queued id acked EXACTLY once over both children, the queue empty, the ack hook found the acked
//! envelope's state in the file at EVERY ack (nothing acked before its zone committed), and the file equals a no-kill
//! reference run on the same seed table by table (BLOBs, nonces, deferred, every `core_outbox` row by content: each
//! envelope's events exactly once, the replays written once). Each loop kills at least once. Nothing is printed but
//! counts and window names.

use std::path::Path;
use std::sync::{Arc, Mutex};

use kourier_db::rusqlite::{Connection, params};
use kourier_threema::failpoints as fp;
use libthreema::protobuf::d2d;
use prost::Message as _;

use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body;
use crate::support::mock::envelope::{Author, ContactDelta, Conv, GroupDelta, MemberChange, Msg};
use crate::support::piped::{self, DEVICE, Mode, Parent, SERVER_GROUP};
use crate::support::{db_path, key};

/// The phone's D2M device id.
const PHONE: u64 = 0x0FA5_0000_0000_0009;
/// The 1:1 peer and the group's creator (contacts in the file), synthetic.
const PEER: &str = "A9KPEER1";
const CREATOR: &str = "A9KCRTR1";
/// The contact the backlog creates, synthetic.
const NEWC: &str = "A9KNEWC1";
/// A sender the file never knows (dropped), synthetic.
const STRANGER: &str = "A9KSTRG1";
const GROUP: u64 = 0x0909_0A0B_0C0D_0E0F;
/// Every world of these loops (the reference too) is this seed: the same backlog byte for byte.
const SEED: u64 = 909;
/// A loop that goes past it is a bug (36 envelopes cannot make more zones).
const MAX_N: u32 = 40;
/// K3: the k-th ack after which the child dies (one fresh world each): early in the first zone, in its middle, and in
/// the second zone (three points keep the test under 10 s, the slow-set line).
const K3_AFTER: [usize; 3] = [1, 14, 33];
/// The child of every run.
const CHILD: &str = "backlog_child";

/// What one envelope of the backlog leaves in the file (the expectation the reference run's totals confirm).
#[derive(Clone, Copy)]
struct Item {
    /// One `core_outbox` row (its events).
    row: bool,
    /// One `deferred` row.
    deferred: bool,
    /// A new D2D nonce (false for a replay).
    fresh: bool,
}

/// The counts of the file that a prefix of the backlog fixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counts {
    rows: usize,
    deferred: usize,
    nonces: usize,
}

fn prefix(items: &[Item], b: usize) -> Counts {
    let head = &items[..b];
    Counts {
        rows: head.iter().filter(|i| i.row).count(),
        deferred: head.iter().filter(|i| i.deferred).count(),
        nonces: head.iter().filter(|i| i.fresh).count(),
    }
}

/// The prefix lengths `b` of the backlog whose state the file's counts are.
fn prefixes_of(items: &[Item], c: Counts) -> Vec<usize> {
    (0..=items.len())
        .filter(|b| prefix(items, *b) == c)
        .collect()
}

/// The file's counts through a read-only second connection (the ack hook calls it while the child writes): ONE
/// statement, so one snapshot of the WAL file (three statements could straddle a commit of the child).
fn counts(file: &Path) -> Result<Counts, String> {
    let conn = kourier_db::open_read_only(file, &key()).map_err(|e| format!("open: {e}"))?;
    let (rows, deferred, nonces): (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM core_outbox) + (SELECT count(*) FROM core_outbox_poison), \
                    (SELECT count(*) FROM deferred), (SELECT count(*) FROM d2d_nonce)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|e| format!("count: {e}"))?;
    let size = |n: i64| usize::try_from(n).map_err(|_| "a negative count".to_owned());
    Ok(Counts {
        rows: size(rows)?,
        deferred: size(deferred)?,
        nonces: size(nonces)?,
    })
}

/// The rows for the core in order: `core_outbox` by seq, then `core_outbox_poison` by seq (the envelopes' bytes).
fn rows(file: &Path) -> Vec<Vec<u8>> {
    let conn: Connection = kourier_db::open_read_only(file, &key()).expect("open the file");
    let mut out = Vec::new();
    for sql in [
        "SELECT envelope FROM core_outbox ORDER BY seq",
        "SELECT envelope FROM core_outbox_poison ORDER BY seq",
    ] {
        let mut stmt = conn.prepare(sql).expect("prepare");
        let got: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        out.extend(got);
    }
    out
}

/// Every table the receive path can touch, row by row as text (compared in code, never printed).
fn dump(file: &Path) -> Vec<String> {
    let conn: Connection = kourier_db::open_read_only(file, &key()).expect("open the file");
    let mut lines = Vec::new();
    for table in [
        "identity",
        "join_ready",
        "own_profile",
        "settings",
        "contact",
        "grp",
        "dist_list",
        "held_blob",
        "csp_nonce",
        "d2d_nonce",
        "deferred",
        "deferred_total",
        "core_outbox",
        "core_outbox_poison",
    ] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table}"))
            .expect("prepare");
        let columns = stmt.column_count();
        let got: Vec<String> = stmt
            .query_map([], |r| {
                let mut line = format!("{table}:");
                for i in 0..columns {
                    line.push_str(&format!("{:?}|", r.get_ref(i)?));
                }
                Ok(line)
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        lines.extend(got);
    }
    lines.sort();
    lines
}

/// The contacts and the group the backlog's texts need, written into the parent's file before the first child.
fn seed_file(p: &Parent) {
    let db = p.open();
    db.zone(|tx| {
        for id in [PEER, CREATOR] {
            let sync = ContactDelta::create(id, [7; 32], START_MS).build();
            tx.execute(
                "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
                params![id, sync.encode_to_vec()],
            )?;
        }
        let sync = GroupDelta::create(CREATOR, GROUP, "synthetic group", &[], START_MS).build();
        tx.execute(
            "INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES (?1, ?2, ?3, 0)",
            params![
                CREATOR,
                i64::from_ne_bytes(GROUP.to_ne_bytes()),
                sync.encode_to_vec()
            ],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("seed the contacts and the group");
}

/// One envelope of the backlog and what it leaves.
enum Kind {
    Row(d2d::Envelope),
    Deferred(d2d::Envelope),
    Dropped(d2d::Envelope),
    Replay(usize),
}

/// The backlog: sealed by the phone and queued for the NEXT new slot (the first child's), in this order.
fn backlog(p: &Parent) -> Vec<Item> {
    let author = Author::new(PHONE);
    let at = |i: usize| START_MS + u64::try_from(i).expect("an index");
    let text = |i: usize, what: &str| {
        let n = u8::try_from(i + 1).expect("a small index");
        Msg::text([n; 8], at(i), what).nonce([n; 24])
    };
    let mut kinds: Vec<Kind> = vec![
        Kind::Row(author.contact_create(
            ContactDelta::create(NEWC, [5; 32], START_MS).names("Synthetic", "Kill"),
        )),
        Kind::Row(
            author.group_update(
                GroupDelta::new(CREATOR, GROUP)
                    .members(&[PEER])
                    .name("Synthetic kill team"),
                &[(PEER, MemberChange::Added)],
            ),
        ),
        Kind::Deferred(author.incoming(
            PEER,
            &Msg::raw([3; 8], at(2), body::ty::POLL_VOTE, body::filler(24, 3)).nonce([3; 24]),
        )),
        Kind::Dropped(author.incoming(STRANGER, &text(3, "synthetic stranger"))),
        Kind::Row(author.incoming(PEER, &text(4, "synthetic 1:1"))),
        Kind::Row(author.incoming(PEER, &text(5, "synthetic group").in_group(CREATOR, GROUP))),
        Kind::Row(author.outgoing(&Conv::contact(PEER), &text(6, "synthetic out"))),
        Kind::Row(author.incoming(
            PEER,
            &Msg::edit([8; 8], at(7), [5; 8], "synthetic edit").nonce([8; 24]),
        )),
        Kind::Row(author.incoming(NEWC, &text(8, "synthetic new contact"))),
        Kind::Replay(4),
    ];
    for i in 10..34 {
        kinds.push(Kind::Row(match i % 4 {
            0 => author.incoming(PEER, &text(i, "synthetic 1:1")),
            1 => author.incoming(PEER, &text(i, "synthetic group").in_group(CREATOR, GROUP)),
            2 => author.incoming(NEWC, &text(i, "synthetic new contact")),
            _ => author.outgoing(&Conv::contact(PEER), &text(i, "synthetic out")),
        }));
    }
    kinds.push(Kind::Replay(5));
    kinds.push(Kind::Row(
        author.incoming(PEER, &text(35, "synthetic last")),
    ));

    let mut phone = p.world.phone(&p.dgk, SERVER_GROUP, PHONE);
    let mediator = p.world.mediator();
    let mut sealed: Vec<Vec<u8>> = Vec::new();
    let mut items = Vec::new();
    for kind in &kinds {
        let (bytes, item) = match kind {
            Kind::Row(e) => (phone.seal(e), (true, false, true)),
            Kind::Deferred(e) => (phone.seal(e), (false, true, true)),
            Kind::Dropped(e) => (phone.seal(e), (false, false, true)),
            Kind::Replay(j) => (sealed[*j].clone(), (false, false, false)),
        };
        mediator.reflect_during_join(0, bytes.clone());
        sealed.push(bytes);
        items.push(Item {
            row: item.0,
            deferred: item.1,
            fresh: item.2,
        });
    }
    assert!(items.len() > 30, "the backlog must span two zones");
    items
}

/// What the fake's ack hook saw: per ack, the reflect id, whether the queue held it, and the file's counts then.
type Seen = Arc<Mutex<Vec<(u32, bool, Result<Counts, String>)>>>;

/// One world of a loop: the parent, the backlog's table, the hook's list.
struct World {
    p: Parent,
    items: Vec<Item>,
    seen: Seen,
}

/// A fresh world on [`SEED`] whose ack hook records the file's counts at every ack.
fn world() -> World {
    let p = piped::parent(SEED);
    seed_file(&p);
    let items = backlog(&p);
    let seen: Seen = Arc::default();
    let (file, list) = (db_path(p.dir()), seen.clone());
    p.world.mediator().on_reflected_ack(move |ack| {
        let state = counts(&file);
        list.lock()
            .expect("the hook's list")
            .push((ack.reflected_id, ack.known, state));
    });
    World { p, items, seen }
}

/// The reflect ids of the slot's queue in order: what the fake sent the FIRST connection after its `ServerInfo`.
fn queue_ids(p: &Parent) -> Vec<u32> {
    let first = p.world.net().opened()[0].0.to_string();
    p.world
        .trace()
        .lines()
        .iter()
        .filter_map(|line| {
            let mut parts = line.split(' ').skip(1);
            if parts.next() != Some(first.as_str())
                || parts.next() != Some("out")
                || parts.next() != Some("reflected")
            {
                return None;
            }
            parts
                .find_map(|kv| kv.strip_prefix("reflected-id="))
                .and_then(|v| v.parse::<u64>().ok())
                .and_then(|v| u32::try_from(v).ok())
        })
        .collect()
}

/// The no-kill run on [`SEED`]: its file (the table dump) and its rows for the core, checked like every other end.
struct Reference {
    dump: Vec<String>,
    rows: Vec<Vec<u8>>,
}

fn reference() -> Option<Reference> {
    let w = world();
    let ended = w.p.spawn(module_path!(), CHILD, Mode::Resume, None)?.wait();
    assert!(
        ended.passed(),
        "the reference child failed: {ended:?}\n{}",
        ended.stderr
    );
    let file = db_path(w.p.dir());
    let total = prefix(&w.items, w.items.len());
    assert_eq!(
        counts(&file),
        Ok(total),
        "the reference run did not leave the backlog's table (rows, deferred, nonces)"
    );
    let reference = Reference {
        dump: dump(&file),
        rows: rows(&file),
    };
    assert!(
        reference.rows.windows(2).all(|w| w[0] != w[1]) && {
            let mut sorted = reference.rows.clone();
            sorted.sort();
            sorted.windows(2).all(|w| w[0] != w[1])
        },
        "two rows of the reference are equal: rows could not be told apart by content"
    );
    check_end(&w, &reference, "reference");
    w.p.world.violations().assert_empty();
    Some(reference)
}

/// The window of a loop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Window {
    /// K1: inside the zone.
    Inside,
    /// K2: after the commit, before the acks.
    After,
    /// K3: after some acks.
    Acks,
}

/// After the FIRST child's kill: the acks are a prefix of the queue, the queue holds the rest, and the file is the
/// state of a prefix of the backlog as the window says, its rows the reference's first rows.
fn check_kill(w: &World, reference: &Reference, window: Window, what: &str) {
    let ids = queue_ids(&w.p);
    let n = w.items.len();
    assert_eq!(
        ids.len(),
        n,
        "{what}: the first child did not get the whole backlog"
    );
    let seen = w.seen.lock().expect("the hook's list").clone();
    let a = seen.len();
    assert!(
        seen.iter()
            .zip(&ids)
            .all(|((id, known, _), queued)| *known && id == queued),
        "{what}: the acks before the kill are not a prefix of the queue in order"
    );
    assert!(a <= n, "{what}: more acks than queued envelopes");
    assert_eq!(
        w.p.world
            .mediator()
            .queue_len(&w.p.keys.path_public, DEVICE),
        Some(n - a),
        "{what}: the queue does not hold exactly the un-acked envelopes"
    );
    let file = db_path(w.p.dir());
    let state = counts(&file).expect("read the killed child's file");
    let at = prefixes_of(&w.items, state);
    let held = match window {
        Window::Inside => at.contains(&a),
        Window::After => at.iter().any(|b| *b > a),
        Window::Acks => at.iter().any(|b| *b >= a),
    };
    assert!(
        held,
        "{what}: after {a} acks the file holds {state:?}, not the state the window allows"
    );
    let got = rows(&file);
    assert!(
        got.len() <= reference.rows.len() && got[..] == reference.rows[..got.len()],
        "{what}: the killed child's rows are not the reference's first rows"
    );
}

/// After the LAST child: every queued id acked exactly once, the queue empty, each ack found its envelope's state in
/// the file (the zone committed first), and the file equals the reference table by table.
fn check_end(w: &World, reference: &Reference, what: &str) {
    let ids = queue_ids(&w.p);
    let n = w.items.len();
    assert_eq!(
        ids.len(),
        n,
        "{what}: the first child did not get the whole backlog"
    );
    let seen = w.seen.lock().expect("the hook's list").clone();
    assert!(
        seen.iter().all(|(_, known, _)| *known),
        "{what}: an ack named an id the queue did not hold"
    );
    let mut acked: Vec<u32> = seen.iter().map(|(id, _, _)| *id).collect();
    acked.sort_unstable();
    let mut queued = ids.clone();
    queued.sort_unstable();
    assert!(
        acked == queued,
        "{what}: {} acks for {n} queued envelopes: not every one exactly once",
        seen.len()
    );
    assert_eq!(
        w.p.world
            .mediator()
            .queue_len(&w.p.keys.path_public, DEVICE),
        Some(0),
        "{what}: the queue is not empty"
    );
    for (k, (id, _, state)) in seen.iter().enumerate() {
        let j = ids.iter().position(|q| q == id).expect("a queued id");
        let state = state
            .clone()
            .unwrap_or_else(|e| panic!("{what}: ack {k}: the hook could not read the file: {e}"));
        assert!(
            prefixes_of(&w.items, state).iter().any(|b| *b > j),
            "{what}: ack {k} (queue position {j}) came while the file held {state:?}: acked before its zone committed"
        );
    }
    let file = db_path(w.p.dir());
    assert!(
        rows(&file) == reference.rows,
        "{what}: the rows for the core are not the reference's (each envelope's events exactly once)"
    );
    assert!(
        dump(&file) == reference.dump,
        "{what}: the file is not the reference's (BLOBs, nonces, deferred, rows)"
    );
}

/// The second child on the same world: runs to `Syncing` (after the backlog) and ends; then the end's checks.
fn second_child(w: &World, reference: &Reference, what: &str) {
    let ended =
        w.p.spawn(module_path!(), CHILD, Mode::Resume, None)
            .expect("unshare was there for the first child")
            .wait();
    assert!(
        ended.passed(),
        "{what}: the second child failed: {ended:?}\n{}",
        ended.stderr
    );
    check_end(w, reference, what);
    w.p.world.violations().assert_empty();
}

/// The failpoint loop of `point` (K1 or K2): n = 1, 2, ... on fresh worlds until the child passes. The kills and
/// the last world.
fn failpoint_loop(point: &str, window: Window) -> Option<(u32, Parent)> {
    let reference = reference()?;
    for n in 1..=MAX_N + 1 {
        assert!(n <= MAX_N, "{point} is hit more than {MAX_N} times");
        let w = world();
        let armed = format!("{point}:{n}");
        let ended =
            w.p.spawn(module_path!(), CHILD, Mode::Resume, Some(&armed))
                .expect("unshare was there for the reference")
                .wait();
        if ended.passed() {
            // The n-th zone never came: this run is a clean one.
            check_end(&w, &reference, &armed);
            w.p.world.violations().assert_empty();
            // Every n before this one killed (an n that did not would have ended the loop).
            let kills = n - 1;
            assert!(kills >= 1, "{point} was never hit: the loop saw nothing");
            eprintln!("{point}: {kills} kills, one per zone of the first child");
            return Some((kills, w.p));
        }
        assert!(
            ended.aborted_at_failpoint(),
            "{armed}: the child neither passed nor died at the failpoint: {ended:?}\n{}",
            ended.stderr
        );
        check_kill(&w, &reference, window, &armed);
        second_child(&w, &reference, &armed);
    }
    unreachable!("the loop ends by its assertion")
}

/// CHILD of every run: the actor connects and runs until `Syncing` (the CSP login-ack, which the fake answers only
/// after the backlog and Dry: every reflected envelope before it is in a committed and acked zone), then shuts down.
/// Killed at a failpoint or by the parent's hook on the way.
#[tokio::test]
async fn backlog_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut c = piped::child().await;
    c.syncing().await;
    c.finish().await;
}

/// K1: a child killed INSIDE the n-th batch zone leaves nothing of that zone in the file and no ack of it in the
/// fake (the acks are exactly the zones before); the next child applies the redelivery once.
#[test]
fn a_backlog_killed_inside_a_zone_is_applied_once_by_the_next_child() {
    let Some((kills, last)) = failpoint_loop(fp::INSIDE_REFLECTED, Window::Inside) else {
        return;
    };
    assert!(kills >= 1, "K1 never killed");
    last.world.violations().assert_empty();
}

/// K2: a child killed AFTER the n-th zone committed and before its acks leaves the whole zone in the file and no
/// ack of it; the next child acks the redelivery and writes no second row.
#[test]
fn a_backlog_killed_after_a_commit_before_its_acks_is_acked_once_and_written_once() {
    let Some((kills, last)) = failpoint_loop(fp::AFTER_REFLECTED, Window::After) else {
        return;
    };
    assert!(kills >= 1, "K2 never killed");
    last.world.violations().assert_empty();
}

/// K3: the child dies at `threema_after_reflected_ack:k`, right after its k-th ack went out (on the send path, after
/// some acks of a zone and before the rest): the k acks stay, the rest is redelivered to the next child and acked
/// once, nothing written twice.
#[test]
fn a_backlog_killed_after_some_acks_is_acked_once_and_written_once() {
    let Some(reference) = reference() else {
        return;
    };
    let mut last = None;
    for k in K3_AFTER {
        let w = world();
        let armed = format!("{}:{k}", fp::AFTER_REFLECTED_ACK);
        let ended =
            w.p.spawn(module_path!(), CHILD, Mode::Resume, Some(&armed))
                .expect("unshare was there for the reference")
                .wait();
        assert!(
            ended.aborted_at_failpoint(),
            "{armed}: the child did not die after its k-th ack: {ended:?}\n{}",
            ended.stderr
        );
        let acked = w.seen.lock().expect("the hook's list").len();
        assert!(
            acked == k && k < w.items.len(),
            "{armed}: the fake took {acked} acks, not the k before the kill"
        );
        check_kill(&w, &reference, Window::Acks, &armed);
        second_child(&w, &reference, &armed);
        last = Some(w.p);
    }
    eprintln!("K3: {} kills, each after exactly k acks", K3_AFTER.len());
    let last = last.expect("K3 ran");
    last.world.violations().assert_empty();
}
