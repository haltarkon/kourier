//! G1f: what message search costs at scale, on SQLCipher (critic M1, M5, M8, M11, H4; the
//! brief's section 2.9). Every test here is `#[ignore]`: it runs only by name, in a release
//! build, under a timeout and a memory watchdog (`.claude/rules/testing.md`):
//!
//! ```text
//! cargo nextest run --release -p kourier-core -p kourier-viewmodel --run-ignored only \
//!     --no-capture -E 'package(kourier-core) and test(/^search_measure::/)'
//! ```
//!
//! The seed is direct (not through the core and not in xtask, M5): streaming batches of raw
//! `INSERT` into `messages` of a SQLCipher core.db (the triggers log each row), then the real
//! flush, backfill and compaction functions through `testing`, which write search.db (the fix
//! round: the index has its own file; the query walks search.db and looks up in core.db). The
//! "week block" below predates the hour rowid: G1f's re-measure seeds one HOUR over the cap. The text is synthetic: a
//! Zipf vocabulary of a million words (a fifth of them Cyrillic), 8 to 24 words a message,
//! 500 chats of Zipf sizes, spread over three years. Nothing is a message of anybody. The
//! numbers go to stdout and to `target/next/g1-perf-numbers.md`.
//!
//! `KOURIER_MEASURE_ROWS` (default 2,000,000) shrinks the main world for a smoke run.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, Network};
use kourier_core::facade::{SearchPage, SearchQuery};
use kourier_core::testing::search::Task;
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, remote};
use kourier_model::{ConvKey, ConversationId, MsgIdScope, NetworkKind, Redacted, UserId};

const KEY: [u8; 32] = [9; 32];
const CHATS: usize = 500;
const VOCAB: usize = 1_000_000;
/// One message every 47.3 s: three years for two million messages.
const STEP_MS: i64 = 47_304;
const START_MS: i64 = 1_600_000_000_000;
const HOUR_MS: i64 = 3_600_000;
/// Messages with the word `blockword`, all inside ONE hour (the rowid's bucket since the own-file
/// design): more than the page cap (5,000), so the walk of that hour must resume in id order.
const BLOCK_ROWS: usize = 6_000;
/// The watchdog: the run aborts above this resident size (the machine rule: a runaway
/// process must not take a shared machine down).
const RSS_CAP_KB: u64 = 4 * 1024 * 1024;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn chat_user(i: usize) -> String {
    format!("aci:00000000-0000-4000-8000-{:012x}", 0x1000 + i)
}

fn watchdog() {
    std::thread::spawn(|| {
        loop {
            if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
                let kb = s
                    .lines()
                    .find_map(|l| l.strip_prefix("VmRSS:"))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
                    .unwrap_or(0);
                if kb > RSS_CAP_KB {
                    eprintln!("search_measure: resident size {kb} kB over the cap, aborting");
                    std::process::abort();
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1_u64 << 53) as f64
    }
}

const LC: [&str; 14] = [
    "b", "d", "k", "l", "m", "n", "p", "r", "s", "t", "v", "z", "g", "h",
];
const LV: [&str; 5] = ["a", "e", "i", "o", "u"];
const CC: [&str; 14] = [
    "б", "д", "к", "л", "м", "н", "п", "р", "с", "т", "в", "з", "г", "х",
];
const CV: [&str; 5] = ["а", "е", "і", "о", "у"];

/// The word of rank `k`: syllables of consonant + vowel (70 of them), more of them for the
/// rarer words; every fifth rank is Cyrillic (rank 0 is Latin).
fn word(k: usize) -> String {
    let cyr = k % 5 == 4;
    let (c, v) = if cyr { (&CC, &CV) } else { (&LC, &LV) };
    let mut n = k;
    let syl = if n < 70 {
        1
    } else if n < 70 + 4_900 {
        n -= 70;
        2
    } else if n < 70 + 4_900 + 343_000 {
        n -= 70 + 4_900;
        3
    } else {
        n -= 70 + 4_900 + 343_000;
        4
    };
    let mut digits = Vec::new();
    for _ in 0..syl {
        digits.push(n % 70);
        n /= 70;
    }
    let mut out = String::new();
    for d in digits.iter().rev() {
        out.push_str(c[d / 5]);
        out.push_str(v[d % 5]);
    }
    out
}

fn is_latin(k: usize) -> bool {
    k % 5 != 4
}

struct Seeded {
    chats: Vec<ConversationId>,
    counts: Vec<u8>,
    small_words: HashSet<usize>,
    block_hour: i64,
    block_rows: usize,
    rows: i64,
    secs: f64,
}

struct World {
    _dir: tempfile::TempDir,
    path: PathBuf,
    h: IngestHarness,
    seeded: Seeded,
}

fn said(who: &str, local: &str, n: i64, text: &str) -> BackendEvent {
    let k = ConvKey::Direct(uid(who));
    BackendEvent::Message(remote(&k, &uid(who), local, T0 + n * 1000, text))
}

fn dbkey() -> kourier_db::DbKey {
    kourier_db::DbKey::from(KEY)
}

/// A migrated SQLCipher file with `rows` synthetic messages written straight into
/// `messages` (the triggers queue them), 500 chats, nothing indexed yet.
fn seed(rows: i64, pgsz: Option<i64>) -> World {
    let t0 = Instant::now();
    let dir = tempfile::tempdir_in(
        std::env::var_os("KOURIER_MEASURE_DIR").unwrap_or_else(|| "/var/tmp".into()),
    )
    .expect("tempdir");
    let path = dir.path().join("core.db");
    let me = chat_user(9_999);
    let mut h = IngestHarness::open(
        &path,
        &dbkey(),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(&me)],
    )
    .expect("harness");
    if let Some(p) = pgsz {
        h.search_db()
            .execute(
                "INSERT INTO messages_fts(messages_fts, rank) VALUES ('pgsz', ?1)",
                [p],
            )
            .expect("pgsz");
    }
    for i in 0..CHATS {
        let who = chat_user(i);
        h.apply(vec![said(&who, "open", i as i64, "chatopener")])
            .expect("chat");
    }
    let chats: Vec<ConversationId> = h
        .connection()
        .prepare("SELECT id FROM conversations ORDER BY id")
        .expect("prepare")
        .query_map([], |r| r.get(0).map(ConversationId))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("ids");
    assert_eq!(chats.len(), CHATS);
    let account = h.account().0;
    println!(
        "search_measure: {CHATS} chats opened in {:.1} s",
        t0.elapsed().as_secs_f64()
    );

    // Cumulative tables: Zipf words (s = 1) and chats (s = 0.7).
    let mut cum = Vec::with_capacity(VOCAB);
    let mut acc = 0.0_f64;
    for r in 0..VOCAB {
        acc += 1.0 / (r as f64 + 1.0);
        cum.push(acc);
    }
    let total_w = acc;
    let mut ccum = Vec::with_capacity(CHATS);
    let mut cacc = 0.0_f64;
    for i in 0..CHATS {
        cacc += 1.0 / (i as f64 + 1.0).powf(0.7);
        ccum.push(cacc);
    }
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    let mut counts = vec![0_u8; VOCAB];
    let mut small_words = HashSet::new();
    let block_hour = (START_MS + 1_200_000 * STEP_MS) / HOUR_MS;
    let mut tally = vec![0_usize; CHATS];
    let conn = h.connection();
    let mut i: i64 = 0;
    while i < rows {
        let tx = conn.unchecked_transaction().expect("transaction");
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
                         origin, sort_key, sent_at, received_at, body_kind, body)
                       VALUES (?1, ?2, 0, ?3, ?4, 'Remote', ?5 * 1024, ?5, ?5, 'Text', ?6)",
                )
                .expect("prepare");
            let end = (i + 20_000).min(rows);
            while i < end {
                let u = rng.unit() * cacc;
                let c = ccum.partition_point(|x| *x < u).min(CHATS - 1);
                tally[c] += 1;
                let ms = START_MS + i * STEP_MS;
                let n = 8 + (rng.next() % 17) as usize;
                let mut text = String::with_capacity(n * 8);
                for w in 0..n {
                    let u = rng.unit() * total_w;
                    let r = cum.partition_point(|x| *x < u).min(VOCAB - 1);
                    counts[r] = counts[r].saturating_add(1);
                    if c == CHATS - 1 {
                        small_words.insert(r);
                    }
                    if w > 0 {
                        text.push(' ');
                    }
                    text.push_str(&word(r));
                }
                let body = format!("{{\"Text\":{{\"body\":\"{text}\"}}}}");
                insert
                    .execute((account, chats[c].0, chat_user(c), format!("m{i}"), ms, body))
                    .expect("insert");
                i += 1;
            }
        }
        tx.commit().expect("commit");
    }
    // The dense hour: BLOCK_ROWS messages 500 ms apart (50 minutes), every one with the word.
    {
        let tx = conn.unchecked_transaction().expect("transaction");
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
                         origin, sort_key, sent_at, received_at, body_kind, body)
                       VALUES (?1, ?2, 0, ?3, ?4, 'Remote', ?5 * 1024, ?5, ?5, 'Text', ?6)",
                )
                .expect("prepare");
            for k in 0..BLOCK_ROWS {
                let c = k % 50;
                let ms = block_hour * HOUR_MS + k as i64 * 500;
                let text = format!("blockword {} {}", word(k % 300), word(1 + k % 40));
                let body = format!("{{\"Text\":{{\"body\":\"{text}\"}}}}");
                insert
                    .execute((account, chats[c].0, chat_user(c), format!("b{k}"), ms, body))
                    .expect("insert");
            }
        }
        tx.commit().expect("commit");
    }
    let block_rows = BLOCK_ROWS;
    let secs = t0.elapsed().as_secs_f64();
    let (biggest, smallest, small_rows): (i64, i64, i64) = h
        .connection()
        .query_row(
            "SELECT max(n), min(n), (SELECT count(*) FROM messages WHERE conversation_id = ?1) FROM (SELECT count(*) n FROM messages GROUP BY conversation_id)",
            [chats[CHATS - 1].0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("sizes");
    println!(
        "search_measure: tally 0:{} 1:{} 2:{} 250:{} 499:{} cacc {cacc:.2}",
        tally[0], tally[1], tally[2], tally[250], tally[499]
    );
    println!(
        "search_measure: chat sizes: biggest {biggest}, smallest {smallest}, the small chat {small_rows} messages, {} distinct words in it; seeded in {secs:.0} s",
        small_words.len()
    );
    World {
        _dir: dir,
        path,
        h,
        seeded: Seeded {
            chats,
            counts,
            small_words,
            block_hour,
            block_rows,
            rows,
            secs,
        },
    }
}

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_default()
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

/// `search_queue`'s rows and its pages in core.db (MB).
fn queue_stats(conn: &kourier_db::rusqlite::Connection) -> (i64, f64) {
    let n: i64 = conn
        .query_row("SELECT count(*) FROM search_queue", [], |r| r.get(0))
        .expect("queue");
    let mb: f64 = conn
        .query_row(
            "SELECT coalesce(sum(pgsize), 0) FROM dbstat WHERE name = 'search_queue'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("queue size") as f64
        / 1_048_576.0;
    (n, mb)
}

/// One number of search.db's `meta`, read on a short-lived read-only connection.
fn search_meta(core_db: &Path, key: &str) -> i64 {
    let c = kourier_db::open_read_only(&search::beside(core_db), &dbkey()).expect("meta reader");
    search::meta(&c, key).expect("meta")
}

#[derive(Default)]
struct Lat {
    ingest: Vec<f64>,
    send: Vec<f64>,
    mark: Vec<f64>,
    q: Vec<f64>,
    q_capped: usize,
    q_none: usize,
}

/// Core jobs through the real store every 100 ms (ingest of one message, send of one message,
/// a read mark, in turn) and a query every second, until `stop` says so. Every ingested
/// message carries a word of its own, pushed on `words`.
#[allow(clippy::too_many_arguments)]
fn drive(
    task: &Task,
    net: &Arc<dyn Network>,
    conv: ConversationId,
    who: &str,
    seq: &mut i64,
    qtexts: &[String],
    lat: &mut Lat,
    words: &mut Vec<String>,
    stop: &mut dyn FnMut() -> bool,
) {
    let t0 = Instant::now();
    let mut tick = 0_u32;
    loop {
        if stop() {
            return;
        }
        let due = t0 + Duration::from_millis(100 * u64::from(tick));
        if let Some(d) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }
        tick += 1;
        *seq += 1;
        let now = 1_700_000_000_000 + *seq;
        match *seq % 3 {
            0 => {
                let word = format!("durw{}x", *seq);
                let d = task
                    .ingest(
                        vec![said(
                            who,
                            &format!("d{}", *seq),
                            1_000 + *seq,
                            &format!("{word} filler words"),
                        )],
                        now,
                    )
                    .expect("ingest");
                lat.ingest.push(d.as_secs_f64() * 1000.0);
                words.push(word);
            }
            1 => {
                let d = task
                    .send(net.clone(), conv, "a message of mine", now)
                    .expect("send");
                lat.send.push(d.as_secs_f64() * 1000.0);
            }
            _ => {
                let (d, _) = task.mark_read(conv, now).expect("mark read");
                lat.mark.push(d.as_secs_f64() * 1000.0);
            }
        }
        if tick % 10 == 5 && !qtexts.is_empty() {
            let text = &qtexts[(tick as usize / 10) % qtexts.len()];
            match task.query(ask(text, None, 50)).expect("query") {
                Some((page, _, d)) => {
                    lat.q.push(d.as_secs_f64() * 1000.0);
                    lat.q_capped += usize::from(page.capped);
                }
                None => lat.q_none += 1,
            }
        }
    }
}

fn file_mb(path: &Path) -> (f64, f64) {
    let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0;
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    (len(path), len(Path::new(&wal)))
}

fn sizes(conn: &kourier_db::rusqlite::Connection) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = conn
        .prepare("SELECT name, sum(pgsize) FROM dbstat GROUP BY name HAVING sum(pgsize) > 1000000 ORDER BY 2 DESC")
        .expect("dbstat")
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as f64 / 1_048_576.0)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("collect");
    v.truncate(8);
    v
}

fn fts_mb(conn: &kourier_db::rusqlite::Connection) -> f64 {
    conn.query_row(
        "SELECT sum(pgsize) FROM dbstat WHERE name LIKE 'messages_fts%'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .expect("fts size") as f64
        / 1_048_576.0
}

fn pages(conn: &kourier_db::rusqlite::Connection) -> (i64, i64) {
    let c: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("pc");
    let f: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .expect("fc");
    (c, f)
}

/// CPU seconds of this process so far (user + system), from `/proc/self/stat`: the number
/// that does not move with the load of the machine.
fn cpu_secs() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = stat.rsplit_once(')').map(|x| x.1).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (ticks(11) + ticks(12)) / 100.0
}

fn next_batch(size: usize, took: Duration) -> usize {
    // The production sizing (`search::index::next_batch`): the job's time against 20 ms.
    let took = took.as_micros().max(1);
    let scaled = (size as u128 * 20_000 / took) as usize;
    scaled.min(size * 2).clamp(25, 500)
}

fn pctl(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    v[((v.len() as f64 * p).ceil() as usize).clamp(1, v.len()) - 1]
}

struct Flush {
    rows: usize,
    secs: f64,
    cpu: f64,
    jobs: Vec<f64>,
}

/// The real flush with the production job sizing; every job is timed whole (COMMIT inside).
fn flush(h: &mut IngestHarness) -> Flush {
    let t0 = Instant::now();
    let c0 = cpu_secs();
    let mut batch = 100;
    let (mut rows, mut jobs) = (0, Vec::new());
    loop {
        let t = Instant::now();
        let f = h.search_flush_one(batch).expect("flush");
        let d = t.elapsed();
        jobs.push(d.as_secs_f64() * 1000.0);
        rows += f.taken;
        batch = next_batch(batch, d);
        if !f.more {
            break;
        }
    }
    Flush {
        rows,
        secs: t0.elapsed().as_secs_f64(),
        cpu: cpu_secs() - c0,
        jobs,
    }
}

fn ask(text: &str, chats: Option<Vec<ConversationId>>, limit: u32) -> SearchQuery {
    SearchQuery {
        text: Redacted::new(text.to_owned()),
        chats,
        limit,
        before: None,
        mode: Default::default(),
    }
}

struct Row {
    label: String,
    n: usize,
    p50: f64,
    p95: f64,
    max: f64,
    min_hits: usize,
    max_looked: usize,
    capped: usize,
    cpu_ms: f64,
}

/// Runs each query once untimed (warm) and `reps` times timed, newest 50; a query that finds
/// nothing fails the row (the guard against a vacuous measurement).
/// A read connection to each file, as the core's search reader has them.
struct Readers {
    search: kourier_db::rusqlite::Connection,
    core: kourier_db::rusqlite::Connection,
}

fn readers(core_db: &Path) -> Readers {
    Readers {
        search: kourier_db::open_read_only(&search::beside(core_db), &dbkey()).expect("reader"),
        core: kourier_db::open_read_only(core_db, &dbkey()).expect("reader"),
    }
}

fn run_set(
    r: &Readers,
    label: &str,
    qs: &[(String, Option<Vec<ConversationId>>)],
    reps: usize,
) -> Row {
    assert!(!qs.is_empty(), "{label}: no query to run");
    let (mut ms, mut min_hits, mut max_looked, mut capped) = (Vec::new(), usize::MAX, 0, 0);
    let c0 = cpu_secs();
    for (text, chats) in qs {
        let q = ask(text, chats.clone(), 50);
        let (page, looked) = search::page(&r.search, &r.core, &q, search::CAP).expect("page");
        assert!(!page.hits.is_empty(), "{label}: a query returned no row");
        for _ in 0..reps {
            let t = Instant::now();
            let (page, looked) = search::page(&r.search, &r.core, &q, search::CAP).expect("page");
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(!page.hits.is_empty(), "{label}: a query returned no row");
            min_hits = min_hits.min(page.hits.len());
            max_looked = max_looked.max(looked);
            capped += usize::from(page.capped);
        }
        max_looked = max_looked.max(looked);
    }
    let max = ms.iter().cloned().fold(0.0, f64::max);
    let n = ms.len();
    // The warm-up runs are in the CPU figure too: one extra run per query.
    let cpu_ms = (cpu_secs() - c0) * 1000.0 / (n + qs.len()) as f64;
    Row {
        label: label.to_owned(),
        n,
        p50: pctl(&mut ms, 0.5),
        p95: pctl(&mut ms, 0.95),
        max,
        min_hits,
        max_looked,
        capped,
        cpu_ms,
    }
}

struct Sets {
    common: Vec<(String, Option<Vec<ConversationId>>)>,
    two: Vec<(String, Option<Vec<ConversationId>>)>,
    three: Vec<(String, Option<Vec<ConversationId>>)>,
    rare: Vec<(String, Option<Vec<ConversationId>>)>,
    small: Vec<(String, Option<Vec<ConversationId>>)>,
    scoped_two: Vec<(String, Option<Vec<ConversationId>>)>,
    scoped_common: Vec<(String, Option<Vec<ConversationId>>)>,
}

fn sets(s: &Seeded) -> Sets {
    let small = Some(vec![*s.chats.last().expect("chats")]);
    let none = |t: String| (t, None);
    let common = (0..12).map(|k| none(word(k))).collect();
    let mut twos = Vec::new();
    for k in 1..400 {
        if is_latin(k) {
            let p: String = word(k).chars().take(2).collect();
            if !twos.contains(&p) {
                twos.push(p);
            }
        }
        if twos.len() == 20 {
            break;
        }
    }
    let mut threes = Vec::new();
    for k in 20..4_000 {
        if is_latin(k) && word(k).chars().count() >= 4 {
            let p: String = word(k).chars().take(3).collect();
            if !threes.contains(&p) {
                threes.push(p);
            }
        }
        if threes.len() == 20 {
            break;
        }
    }
    let rare = (400_000..VOCAB)
        .filter(|k| is_latin(*k) && (1..=3).contains(&s.counts[*k]))
        .take(30)
        .map(|k| none(word(k)))
        .collect();
    // The small chat's own words: the 20 rarest overall (a word that needs the scope), and the
    // 2-letter starts of its words (every one has a hit in the scope and millions elsewhere).
    let mut sw: Vec<usize> = s
        .small_words
        .iter()
        .copied()
        .filter(|k| is_latin(*k))
        .collect();
    sw.sort_unstable_by_key(|k| (std::cmp::Reverse(*k), *k));
    let small_q: Vec<_> = sw
        .iter()
        .take(20)
        .map(|k| (word(*k), small.clone()))
        .collect();
    sw.sort_unstable();
    let mut scoped_twos: Vec<String> = Vec::new();
    for k in &sw {
        let p: String = word(*k).chars().take(2).collect();
        if !scoped_twos.contains(&p) {
            scoped_twos.push(p);
        }
        if scoped_twos.len() == 20 {
            break;
        }
    }
    Sets {
        common,
        scoped_two: scoped_twos
            .into_iter()
            .map(|p| (p, small.clone()))
            .collect(),
        scoped_common: (0..5).map(|k| (word(k), small.clone())).collect(),
        two: twos.into_iter().map(none).collect(),
        three: threes.into_iter().map(none).collect(),
        rare,
        small: small_q,
    }
}

fn rows_table(rows: &[Row]) -> String {
    let mut s = String::from(
        "| query set | runs | p50 ms | p95 ms | max ms | fewest hits | most rows looked up | capped | mean CPU ms |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {:.1} | {:.1} | {:.1} | {} | {} | {} | {:.1} |",
            r.label, r.n, r.p50, r.p95, r.max, r.min_hits, r.max_looked, r.capped, r.cpu_ms
        );
    }
    s
}

fn query_rows(r: &Readers, s: &Sets) -> Vec<Row> {
    vec![
        run_set(r, "common word (12 words, rank 0-11)", &s.common, 3),
        run_set(r, "2-letter prefix (20)", &s.two, 3),
        run_set(r, "3-letter prefix (20)", &s.three, 3),
        run_set(r, "rare word (30, 1-3 messages in all)", &s.rare, 3),
        run_set(r, "word of the small chat, scoped (20)", &s.small, 3),
        run_set(
            r,
            "common word, scoped to the small chat (5)",
            &s.scoped_common,
            3,
        ),
        run_set(
            r,
            "2-letter prefix, scoped to the small chat, hits elsewhere (20)",
            &s.scoped_two,
            3,
        ),
    ]
}

fn walk(
    r: &Readers,
    text: &str,
    chats: Option<Vec<ConversationId>>,
    limit: u32,
    cap: usize,
    max_pages: usize,
) -> (Vec<f64>, Vec<i64>, usize, usize) {
    let mut q = ask(text, chats, limit);
    let (mut ms, mut ids, mut capped, mut looked_max) = (Vec::new(), Vec::new(), 0, 0);
    loop {
        let t = Instant::now();
        let (page, looked): (SearchPage, usize) =
            search::page(&r.search, &r.core, &q, cap).expect("page");
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
        ids.extend(page.hits.iter().map(|h| h.message.0));
        capped += usize::from(page.capped);
        looked_max = looked_max.max(looked);
        match page.next {
            Some(c) if ms.len() < max_pages => q.before = Some(c),
            _ => break,
        }
        assert!(ms.len() < 100_000, "the walk does not end");
    }
    (ms, ids, capped, looked_max)
}

#[test]
#[ignore = "a measurement (G1f): run by name, in release, under kcap"]
fn at_scale_on_sqlcipher() {
    watchdog();
    let rows: i64 = std::env::var("KOURIER_MEASURE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# G1f measurements (SQLCipher, release, synthetic text)\n"
    );
    let _ = writeln!(
        out,
        "Build: `cargo nextest run --release`, test `search_measure::at_scale_on_sqlcipher`; main world {rows} messages in {CHATS} chats, one message per {STEP_MS} ms; vocabulary {VOCAB} Zipf words (a fifth Cyrillic), 8-24 words a message. Server load average at the start: {}.\n",
        loadavg()
    );

    // ---- the main world
    let mut w = seed(rows, None);
    let _ = writeln!(out, "## Main world: {} messages\n", w.seeded.rows);
    let _ = writeln!(
        out,
        "- seed (raw INSERT, triggers queue every row): {:.0} s = {:.0} rows/s",
        w.seeded.secs,
        w.seeded.rows as f64 / w.seeded.secs
    );
    let queued = search::queued(w.h.connection()).expect("queued");
    let (f0, wal0) = file_mb(&w.path);
    let _ = writeln!(
        out,
        "- queue after the seed: {queued} rows; file {f0:.0} MB + WAL {wal0:.0} MB"
    );
    let f = flush(&mut w.h);
    let mut jobs = f.jobs.clone();
    let _ = writeln!(
        out,
        "- FLUSH (the real job, production sizing 100..500 rows, COMMIT inside): {} rows in {:.0} s = **{:.0} rows/s** ({:.0} CPU s = {:.0} rows per CPU second); {} jobs; job time p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, longest {:.1} ms",
        f.rows,
        f.secs,
        f.rows as f64 / f.secs,
        f.cpu,
        f.rows as f64 / f.cpu.max(0.01),
        jobs.len(),
        pctl(&mut jobs, 0.5),
        pctl(&mut jobs, 0.95),
        pctl(&mut jobs, 0.99),
        jobs.iter().cloned().fold(0.0, f64::max)
    );
    let segs = search::segments(w.h.search_db()).expect("segments");
    let _ = writeln!(
        out,
        "- segments after the flush (automerge only, no compaction): {segs}"
    );
    let (f1, wal1) = file_mb(w.h.search_path());
    let (pc, fl) = pages(w.h.search_db());
    let _ = writeln!(
        out,
        "- file after the flush: {f1:.0} MB + WAL {wal1:.0} MB; {pc} pages, {fl} free"
    );
    let _ = writeln!(
        out,
        "- FTS5 tables (`dbstat`): {:.0} MB; biggest objects:",
        fts_mb(w.h.search_db())
    );
    for (n, mb) in sizes(w.h.search_db()) {
        let _ = writeln!(out, "  - `{n}` {mb:.0} MB");
    }
    let (ready, pct) = search::progress(w.h.search_db()).expect("progress");
    let _ = writeln!(out, "- index state: ready {ready}, {pct} %\n");
    println!("{out}");

    let reader = readers(&w.path);
    let s = sets(&w.seeded);
    let _ = writeln!(
        out,
        "### Queries on the unmerged index (newest 50, one reader, warm)\n"
    );
    let rowsq = query_rows(&reader, &s);
    out.push_str(&rows_table(&rowsq));
    println!("{out}");

    // cold connection: a fresh reader per query (file cache warm, SQLCipher page cache cold)
    let mut cold = Vec::new();
    for (text, chats) in s.two.iter().take(10) {
        let rd = readers(&w.path);
        let t = Instant::now();
        let (page, _) = search::page(
            &rd.search,
            &rd.core,
            &ask(text, chats.clone(), 50),
            search::CAP,
        )
        .expect("page");
        cold.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(!page.hits.is_empty(), "cold: a query returned no row");
    }
    let _ = writeln!(
        out,
        "\nA 2-letter prefix on a fresh connection (10 queries, open included): p50 {:.0} ms, max {:.0} ms.\n",
        pctl(&mut cold.clone(), 0.5),
        cold.iter().cloned().fold(0.0, f64::max)
    );

    // plans
    for (name, plan) in search::plans(&reader.search, &reader.core).expect("plans") {
        let _ = writeln!(out, "- plan `{name}`: {plan}");
    }

    // a page walk: common word, ten pages of 50 by cursor
    let (ms, _, _, _) = walk(&reader, &word(0), None, 50, search::CAP, 10);
    let mut first10: Vec<f64> = ms.iter().take(10).cloned().collect();
    let _ = writeln!(
        out,
        "- common word, first ten pages of 50 by cursor: p50 {:.1} ms, max {:.1} ms (the whole walk has far more pages, not walked)",
        pctl(&mut first10.clone(), 0.5),
        first10.iter().cloned().fold(0.0, f64::max)
    );
    drop(ms);
    first10.clear();

    // a block over the cap (H4)
    let _ = writeln!(
        out,
        "\n### One hour block over the cap (H4, the bucket is the hour now)\n"
    );
    let t = Instant::now();
    let (ms, ids, capped, looked) = walk(&reader, "blockword", None, 100, search::CAP, usize::MAX);
    let total = t.elapsed().as_secs_f64();
    let distinct: HashSet<i64> = ids.iter().copied().collect();
    assert_eq!(ids.len(), distinct.len(), "a hit came twice");
    assert_eq!(distinct.len(), w.seeded.block_rows, "a hit is missing");
    let mut m = ms.clone();
    let _ = writeln!(
        out,
        "- hour {} holds {} messages with the word; the walk by cursor, 100 a page, cap {}: {} pages, {} capped, every hit once ({} distinct); p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms a page; most rows looked up on a page {}; whole walk {:.2} s",
        w.seeded.block_hour,
        w.seeded.block_rows,
        search::CAP,
        ms.len(),
        capped,
        distinct.len(),
        pctl(&mut m, 0.5),
        pctl(&mut m, 0.95),
        ms.iter().cloned().fold(0.0, f64::max),
        looked,
        total
    );
    let busiest: i64 = reader
        .core
        .query_row(
            "SELECT max(n) FROM (SELECT count(*) n FROM messages GROUP BY sort_key / (1024 * 3600000))",
            [],
            |r| r.get(0),
        )
        .expect("busiest hour");
    let _ = writeln!(
        out,
        "- messages in the busiest hour of the world: {busiest} (cap {})",
        search::CAP
    );
    drop(reader);
    println!("{out}");

    write_numbers(&out);

    // ---- the open of search.db at start (item 5): close, open, decrypt, judge (log check
    // included); warm page cache only (no root here to drop it).
    let mut opens = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        let why = w.h.search_open().expect("open");
        opens.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(why.is_none(), "the file belongs: no rebuild at open");
    }
    let _ = writeln!(
        out,
        "\n### `search::file::open` at start on the 2M file (item 5)\n\n- close + open + decrypt + judge (incl. the log check), warm page cache only: {} ms (3 runs); a cold cache cannot be dropped as non-root\n",
        opens
            .iter()
            .map(|v| format!("{v:.0}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    write_numbers(&out);

    // ---- LOW-5 in release (item 3): the chat DELETE with the three triggers against the same
    // DELETE without them, on copies of core.db, 3 runs each, alternated (W N W N W N).
    let chat0 = w.seeded.chats[0];
    let _ =
        w.h.connection()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    let copy_dir = w.path.parent().expect("dir").join("low5");
    std::fs::create_dir_all(&copy_dir).expect("low5 dir");
    let (mut with_t, mut without_t) = (Vec::new(), Vec::new());
    let _ = writeln!(
        out,
        "### Storage LOW-5 in release: the chat DELETE with and without G1's three triggers (item 3)\n"
    );
    let _ = writeln!(out, "Load at start: {}\n", loadavg());
    for run in 0..6 {
        let triggers = run % 2 == 0;
        let copy = copy_dir.join("core.db");
        std::fs::copy(&w.path, &copy).expect("copy core.db");
        let conn = kourier_db::open(&copy, &dbkey()).expect("open copy");
        if !triggers {
            for name in [
                "messages_search_insert",
                "messages_search_update",
                "messages_search_delete",
            ] {
                conn.execute_batch(&format!("DROP TRIGGER {name}"))
                    .expect("drop trigger");
            }
        }
        let t = Instant::now();
        let n = conn
            .execute("DELETE FROM messages WHERE conversation_id = ?1", [chat0.0])
            .expect("delete");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let queued: i64 = conn
            .query_row("SELECT count(*) FROM search_queue", [], |r| r.get(0))
            .expect("queued");
        let (_, wal) = file_mb(&copy);
        let _ = writeln!(
            out,
            "- run {} {}: {n} rows in {ms:.0} ms; log rows {queued}; WAL {wal:.0} MB",
            run + 1,
            if triggers {
                "WITH triggers"
            } else {
                "WITHOUT triggers"
            }
        );
        if triggers {
            with_t.push(ms);
        } else {
            without_t.push(ms);
        }
        drop(conn);
        for f in ["core.db", "core.db-wal", "core.db-shm"] {
            let _ = std::fs::remove_file(copy_dir.join(f));
        }
    }
    let _ = std::fs::remove_dir_all(&copy_dir);
    let med = |v: &mut Vec<f64>| pctl(v, 0.5);
    let (mw, mn) = (med(&mut with_t.clone()), med(&mut without_t.clone()));
    let _ = writeln!(
        out,
        "\n**LOW-5 release: median {mw:.0} ms with the triggers, {mn:.0} ms without = {:+.1} %** (debug probe of the review: about +9 %)\n",
        (mw / mn - 1.0) * 100.0
    );
    write_numbers(&out);

    // ---- the mass delete: chat 0 (the biggest), one statement; then the flush WITHOUT prune
    // (search writer's jobs), then the prune in its capped jobs (core writer's jobs).
    let _ = writeln!(
        out,
        "### Mass delete (a chat clear), flush and prune jobs (M1, items 4 and 6)\n"
    );
    let (qrows0, qmb0) = queue_stats(w.h.connection());
    let t = Instant::now();
    let deleted =
        w.h.connection()
            .execute("DELETE FROM messages WHERE conversation_id = ?1", [chat0.0])
            .expect("delete");
    let del_ms = t.elapsed().as_secs_f64() * 1000.0;
    let (qpeak, qpeak_mb) = queue_stats(w.h.connection());
    let (cf, cwal) = file_mb(&w.path);
    let _ = writeln!(
        out,
        "- `DELETE FROM messages WHERE conversation_id = <biggest chat>`: {deleted} rows in one statement = **{del_ms:.0} ms** of CORE writer hold (triggers queue every row); `search_queue` {qrows0} rows ({qmb0:.1} MB) before, **{qpeak} rows ({qpeak_mb:.1} MB) at its peak**; core.db {cf:.0} MB + WAL {cwal:.0} MB"
    );
    // flush without prune
    let (mut batch, mut jobs_f, mut taken) = (100, Vec::new(), 0);
    let t0 = Instant::now();
    let last_applied = loop {
        let t = Instant::now();
        let f = w.h.search_flush_only(batch).expect("flush");
        let d = t.elapsed();
        jobs_f.push(d.as_secs_f64() * 1000.0);
        taken += f.taken;
        batch = next_batch(batch, d);
        if !f.more {
            break f.applied;
        }
    };
    let flush_secs = t0.elapsed().as_secs_f64();
    let dead = search::meta(w.h.search_db(), "dead").expect("dead");
    let (qafter_flush, qafter_flush_mb) = queue_stats(w.h.connection());
    let _ = writeln!(
        out,
        "- delete flush (production sizing, no prune between): {taken} rows in {flush_secs:.1} s; {} jobs; SEARCH writer hold p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, **longest {:.1} ms**; `dead` = {dead}; `search_queue` still {qafter_flush} rows ({qafter_flush_mb:.1} MB)",
        jobs_f.len(),
        pctl(&mut jobs_f.clone(), 0.5),
        pctl(&mut jobs_f.clone(), 0.95),
        pctl(&mut jobs_f.clone(), 0.99),
        jobs_f.iter().cloned().fold(0.0, f64::max)
    );
    // the prune in capped jobs
    let mut jobs_p = Vec::new();
    let mut gone_total = 0;
    loop {
        let t = Instant::now();
        let (gone, more) = w.h.search_prune_job(last_applied).expect("prune job");
        jobs_p.push(t.elapsed().as_secs_f64() * 1000.0);
        gone_total += gone;
        if !more || gone == 0 {
            break;
        }
    }
    let (qafter, qafter_mb) = queue_stats(w.h.connection());
    let _ = writeln!(
        out,
        "- prune jobs (capped at 5,000 rows): {} jobs, {gone_total} rows; CORE writer hold p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, **longest {:.1} ms**; `search_queue` after the prunes {qafter} rows ({qafter_mb:.1} MB of pages, the file does not shrink)",
        jobs_p.len(),
        pctl(&mut jobs_p.clone(), 0.5),
        pctl(&mut jobs_p.clone(), 0.95),
        pctl(&mut jobs_p.clone(), 0.99),
        jobs_p.iter().cloned().fold(0.0, f64::max)
    );
    let (f2, wal2) = file_mb(w.h.search_path());
    let (pc, fl) = pages(w.h.search_db());
    let _ = writeln!(
        out,
        "- search.db after the delete flush, BEFORE the compaction: {f2:.0} MB + WAL {wal2:.0} MB ({pc} pages, {fl} free); FTS5 tables {:.0} MB",
        fts_mb(w.h.search_db())
    );
    write_numbers(&out);

    // a copy of the flushed-and-deleted file for the task-driven compaction below
    let floor = search::meta(w.h.search_db(), "floor").expect("floor");
    assert_eq!(floor, 0, "the backfill is over: the 24 hour rule may fire");
    assert!(search::checkpoint(w.h.search_db()).expect("checkpoint"));
    let pre = w.path.parent().expect("dir").join("search.db.pre");
    std::fs::copy(w.h.search_path(), &pre).expect("copy search.db");

    // ---- A: the compaction step by step on the harness (the steps' writer holds, sizes)
    let _ = writeln!(out, "\n### The compaction, step by step (items 4 and 6)\n");
    let t = Instant::now();
    let asked = search::compact_after(w.h.connection()).expect("compact_after");
    let (before_mb, before_wal) = file_mb(w.h.search_path());
    let conn = w.h.search_writer();
    let snapshot = search::begin(conn).expect("begin");
    let (mut steps, mut step_ms) = (0, Vec::new());
    loop {
        let t = Instant::now();
        let worked = search::step(conn).expect("step");
        step_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        if !worked {
            break;
        }
        steps += 1;
    }
    search::finish(conn, snapshot, asked, T0).expect("finish");
    let (mid, midwal) = file_mb(w.h.search_path());
    let t_cp = Instant::now();
    let checkpointed = search::checkpoint(w.h.search_db()).expect("checkpoint");
    let cp_ms = t_cp.elapsed().as_secs_f64() * 1000.0;
    let total = t.elapsed().as_secs_f64();
    let (f3, wal3) = file_mb(w.h.search_path());
    let (pc, fl) = pages(w.h.search_db());
    let _ = writeln!(
        out,
        "- COMPACTION: {steps} steps that worked (+1 empty), {total:.1} s in all, SEARCH writer hold per step p50 {:.0} ms, p95 {:.0} ms, p99 {:.0} ms, **longest {:.0} ms**; truncating checkpoint {cp_ms:.0} ms (went through: {checkpointed})",
        pctl(&mut step_ms.clone(), 0.5),
        pctl(&mut step_ms.clone(), 0.95),
        pctl(&mut step_ms.clone(), 0.99),
        step_ms.iter().cloned().fold(0.0, f64::max)
    );
    let _ = writeln!(
        out,
        "- search.db BEFORE {before_mb:.0} MB + WAL {before_wal:.0} MB; before the final checkpoint {mid:.0} MB + WAL {midwal:.0} MB; **AFTER {f3:.0} MB + WAL {wal3:.0} MB** ({pc} pages, {fl} free = {:.0} MB reusable); FTS5 tables {:.0} MB; segments {}",
        fl as f64 * 4096.0 / 1_048_576.0,
        fts_mb(w.h.search_db()),
        search::segments(w.h.search_db()).expect("segments")
    );
    write_numbers(&out);
    println!("{out}");

    // ---- B: the REAL store and search task on the copy: core jobs while the task compacts
    let World {
        h,
        seeded,
        path,
        _dir,
    } = w;
    let account = h.account();
    drop(h);
    for suffix in ["", "-wal", "-shm"] {
        let mut f = search::beside(&path).into_os_string();
        f.push(suffix);
        let _ = std::fs::remove_file(f);
    }
    std::fs::rename(&pre, search::beside(&path)).expect("restore search.db");
    let _ = writeln!(
        out,
        "\n### Core writes and a query DURING the task's compaction (item 2; the real store and search task, the 24 hour rule)\n"
    );
    let _ = writeln!(out, "Load at start: {}\n", loadavg());
    let task = Task::start(
        &path,
        &dbkey(),
        account,
        MsgIdScope::AccountAuthor,
        1_700_000_000_000,
    )
    .expect("task");
    let started = Instant::now();
    while !task.ready() {
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "search.db judged"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = writeln!(
        out,
        "- the task made search.db ready (open + judge + first flush) in {:.0} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let me = uid(&chat_user(9_999));
    let net: Arc<dyn Network> = Arc::new(kourier_fake::FakeNetwork::new(
        kourier_fake::Flavor::SignalLike,
        kourier_fake::Script::new("measure", vec![me]),
    ));
    let conv = seeded.chats[1];
    let who = chat_user(1);
    let s2 = sets(&seeded);
    let qtexts: Vec<String> = s2.two.iter().map(|(t, _)| t.clone()).collect();
    let mut seq = 0_i64;
    // idle baseline: ~20 s
    let mut idle = Lat::default();
    let idle_until = Instant::now() + Duration::from_secs(20);
    drive(
        &task,
        &net,
        conv,
        &who,
        &mut seq,
        &qtexts,
        &mut idle,
        &mut Vec::new(),
        &mut || Instant::now() >= idle_until,
    );
    // the dead tombstones are there; the 24 hour rule falls due
    assert!(
        search_meta(&path, "dead") > 0,
        "tombstones wait for the compaction"
    );
    let t_start = Instant::now();
    task.advance_clock(Duration::from_secs(25 * 3600));
    let mut during = Lat::default();
    let mut words = Vec::new();
    let mut polls = 0_u32;
    let mut last_poll = Instant::now();
    let mut over = false;
    let limit = Instant::now() + Duration::from_secs(900);
    drive(
        &task,
        &net,
        conv,
        &who,
        &mut seq,
        &qtexts,
        &mut during,
        &mut words,
        &mut || {
            if last_poll.elapsed() >= Duration::from_secs(1) {
                last_poll = Instant::now();
                polls += 1;
                // the compaction is over when `dead` is 0 and the checkpoint is done
                if polls > 2
                    && search_meta(&path, "dead") == 0
                    && search_meta(&path, "checkpoint_due") == 0
                {
                    over = true;
                }
            }
            over || Instant::now() >= limit
        },
    );
    let comp_secs = t_start.elapsed().as_secs_f64();
    assert!(over, "the compaction ended within 15 minutes");
    let (fd, wd) = file_mb(&search::beside(&path));
    let _ = writeln!(
        out,
        "- compaction by the task: {comp_secs:.1} s from the clock's jump to `dead` 0 and the checkpoint; search.db after {fd:.0} MB + WAL {wd:.0} MB"
    );
    let _ = writeln!(
        out,
        "\n| core job (High, the account's lane) | state | n | p50 ms | p95 ms | max ms |\n|---|---|---|---|---|---|"
    );
    for (name, a, b) in [
        ("ingest of one message", &idle.ingest, &during.ingest),
        ("send of one message", &idle.send, &during.send),
        ("read mark", &idle.mark, &during.mark),
    ] {
        for (state, v) in [("search task idle", a), ("during compaction", b)] {
            let mut v = v.clone();
            let _ = writeln!(
                out,
                "| {name} | {state} | {} | {:.2} | {:.2} | {:.2} |",
                v.len(),
                pctl(&mut v.clone(), 0.5),
                pctl(&mut v, 0.95),
                v.iter().cloned().fold(0.0, f64::max)
            );
        }
    }
    let _ = writeln!(
        out,
        "\n| query (2-letter prefix, the store's own reader, 1 s time cap) | state | n | p50 ms | p95 ms | max ms | capped | not ready |\n|---|---|---|---|---|---|---|---|"
    );
    for (state, l) in [("search task idle", &idle), ("during compaction", &during)] {
        let mut v = l.q.clone();
        let _ = writeln!(
            out,
            "| 2-letter prefix | {state} | {} | {:.1} | {:.1} | {:.1} | {} | {} |",
            v.len(),
            pctl(&mut v.clone(), 0.5),
            pctl(&mut v, 0.95),
            l.q.iter().cloned().fold(0.0, f64::max),
            l.q_capped,
            l.q_none
        );
    }
    // the messages written during the compaction must be findable after it
    let mut missing = 0;
    let t = Instant::now();
    for w in &words {
        let mut found = false;
        while t.elapsed() < Duration::from_secs(120) {
            if let Some((page, _, _)) = task.query(ask(w, None, 50)).expect("query")
                && !page.hits.is_empty()
            {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        missing += usize::from(!found);
    }
    let _ = writeln!(
        out,
        "\n- messages ingested during the compaction: {}; findable after it ended (by a word of their own): {} ({} missing); waited {:.1} s in all",
        words.len(),
        words.len() - missing,
        missing,
        t.elapsed().as_secs_f64()
    );
    assert_eq!(
        missing, 0,
        "a message written during the compaction is findable after it"
    );
    let _ = writeln!(out, "- load at the end: {}", loadavg());
    task.shutdown();
    write_numbers(&out);

    // after the delete and the compaction: the same queries on a fresh reader
    let reader = readers(&path);
    let _ = writeln!(
        out,
        "\n### Queries after the delete and the compaction (merged index, chat 0 gone)\n"
    );
    let after = vec![
        run_set(&reader, "common word", &s2.common, 3),
        run_set(&reader, "2-letter prefix", &s2.two, 3),
        run_set(&reader, "3-letter prefix", &s2.three, 3),
        run_set(&reader, "rare word", &s2.rare, 3),
        run_set(
            &reader,
            "2-letter prefix, scoped to the small chat",
            &s2.scoped_two,
            3,
        ),
    ];
    out.push_str(&rows_table(&after));
    drop(reader);
    drop(_dir);

    write_numbers(&out);
    println!("{out}");
}

fn write_numbers(out: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/next");
    std::fs::create_dir_all(&dir).expect("target/next");
    let name =
        std::env::var("KOURIER_MEASURE_FILE").unwrap_or_else(|_| "g1-perf-numbers.md".into());
    std::fs::write(dir.join(name), out).expect("write numbers");
}

/// `pgsz` 4050 (FTS5's default) against 3900 (ours) on two 500,000-message files, and the
/// backfill's rate on 250,000 old messages.
#[test]
#[ignore = "a measurement (G1f): run by name, in release, under kcap"]
fn pgsz_and_backfill() {
    watchdog();
    let rows: i64 = std::env::var("KOURIER_MEASURE_AB_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500_000);
    let mut out = String::new();
    let _ = writeln!(out, "# G1f: pgsz A/B and the backfill rate\n");
    let ab: &[i64] = if std::env::var_os("KOURIER_MEASURE_PGSZ").is_some() {
        &[3900, 4050]
    } else {
        &[]
    };
    for &pgsz in ab {
        let mut w = seed(rows, Some(pgsz));
        let f = flush(&mut w.h);
        let mut jobs = f.jobs.clone();
        let (mb, wal) = file_mb(w.h.search_path());
        let _ = writeln!(
            out,
            "## pgsz {pgsz}, {rows} messages\n\n- flush {:.0} rows/s ({:.0} s), job p95 {:.1} ms, longest {:.1} ms; file {mb:.0} MB + WAL {wal:.0} MB; FTS5 tables {:.0} MB; segments {}\n",
            f.rows as f64 / f.secs,
            f.secs,
            pctl(&mut jobs, 0.95),
            jobs.iter().cloned().fold(0.0, f64::max),
            fts_mb(w.h.search_db()),
            search::segments(w.h.search_db()).expect("segments")
        );
        let reader = readers(&w.path);
        let s = sets(&w.seeded);
        let table = rows_table(&[
            run_set(&reader, "common word", &s.common, 3),
            run_set(&reader, "2-letter prefix", &s.two, 3),
            run_set(&reader, "3-letter prefix", &s.three, 3),
            run_set(&reader, "rare word", &s.rare, 3),
            run_set(
                &reader,
                "2-letter prefix, scoped to the small chat",
                &s.scoped_two,
                3,
            ),
        ]);
        out.push_str(&table);
        out.push('\n');
        println!("{out}");
    }
    // the backfill: the rows are old (below `floor`), the queue empty, as after the migration
    let n = rows / 2;
    let mut w = seed(n, None);
    // A file built again: every row at or below `floor`, the log applied.
    w.h.search_rebuild().expect("old rows");
    let t0 = Instant::now();
    let c0 = cpu_secs();
    let (mut batch, mut looked, mut jobs) = (100, 0, Vec::new());
    loop {
        let t = Instant::now();
        let f = w.h.search_backfill_one(batch).expect("backfill");
        let d = t.elapsed();
        jobs.push(d.as_secs_f64() * 1000.0);
        looked += f.looked;
        batch = next_batch(batch, d);
        if f.floor == 0 {
            break;
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    let cpu = cpu_secs() - c0;
    let mut j = jobs.clone();
    let mean_job = jobs.iter().sum::<f64>() / jobs.len() as f64;
    let paced = looked as f64 / (jobs.len() as f64 * (mean_job + 80.0) / 1000.0);
    let _ = writeln!(
        out,
        "## Backfill, {n} old messages\n\n- {looked} rows read in {secs:.0} s = **{:.0} rows/s unpaced** ({cpu:.0} CPU s = **{:.0} rows per CPU second**; production sizing 100..500); {} jobs, job p50 {:.1} ms, p95 {:.1} ms, longest {:.1} ms; with the task's 80 ms pause between jobs about **{paced:.0} rows/s**, so 2,000,000 old messages take about {:.0} minutes",
        looked as f64 / secs,
        looked as f64 / cpu.max(0.01),
        jobs.len(),
        pctl(&mut j, 0.5),
        pctl(&mut j, 0.95),
        jobs.iter().cloned().fold(0.0, f64::max),
        2_000_000.0 / paced / 60.0
    );
    let (ready, pct) = search::progress(w.h.search_db()).expect("progress");
    let _ = writeln!(
        out,
        "- state after: ready {ready}, {pct} %; file {:.0} MB; FTS5 tables {:.0} MB",
        file_mb(w.h.search_path()).0,
        fts_mb(w.h.search_db())
    );
    drop(w);
    let name =
        std::env::var("KOURIER_MEASURE_FILE_AB").unwrap_or_else(|_| "g1-perf-ab-numbers.md".into());
    write_numbers_named(&out, &name);
    println!("{out}");
}

fn write_numbers_named(out: &str, name: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/next");
    std::fs::create_dir_all(&dir).expect("target/next");
    std::fs::write(dir.join(name), out).expect("write numbers");
}
