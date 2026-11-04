//! The group fetch policy (W6a2, chunk B6; decision D9 and the critic's M1 of `target/wa/w6-decisions.md`): WHEN Kourier asks
//! the server about groups. Every group IQ of the crate is made here, by ONE task per connection, and every one takes its
//! place in ONE bucket.
//!
//! WHAT IS ASKED, AND WHEN:
//! - AT EACH START, after the connection came `Online` and never before (the task is started at that moment): ONE
//!   `participating` IQ (the slim overview: ids, no members) and ONE batch IQ of every group it lists (D9: two IQs, the
//!   shape of [`Session::fetch_groups`]). The pair counts as ONE fetch of the bucket. An answered batch is stated in one
//!   zone ([`crate::groups::zone::fetched_by_group`]); a group the batch does not name keeps its row and its mark (scout T3). A
//!   batch that failed is ONE failed IQ for every group in it (scout T4): nothing falls back to single queries and
//!   nothing is asked again in this start, the next start's batch is the retry. A read that never reached the server or
//!   got no answer in time (no client, not connected, a timeout, a zone that failed) ends the task and is asked again at
//!   the NEXT `Online` of the start, as the blocklist is.
//! - THEN, one single query (`fetch_group`) for a group the batch answered `Truncated` (M1: once per start) and for a group
//!   a notice the event cannot carry, or a `groups_dirty`, marked for a fetch (`group_state.dirty_at`, written by the
//!   handler's zone: [`crate::groups::zone::apply`], `EventWriter::write_groups_dirty_and_wait`). The marks are looked at
//!   when the event writer says one was made. A truncated group is tried ONCE per start whatever came of it; a MARKED group
//!   is asked again in the same start when a mark newer than its last single query was made since (fix F17, DUR-3: a
//!   session of days must not hold a second notice for the next app start), under the same bucket, which caps any loop. A
//!   mark not newer than that query is not asked again (a refusal or an unreadable answer leaves it for the next start; a
//!   read that did not reach the server ends the task and the group is tried at the next `Online`).
//!
//! THE BUCKET ([`Bucket`], M1): at most ONE fetch per [`FLOOR`] (30 s) and [`PER_WINDOW`] (10) per [`WINDOW`] (10 min),
//! for the whole account, however many groups there are or how often the connection comes back: a task that wants a
//! fetch waits for its slot on the account's clock (the bucket lives as long as the actor, not as long as the client).
//! A server that refused with a delay (`backoff`, or a 429 with none) closes the bucket for that delay: the next fetch
//! waits for it. A burst of notices, a `groups_dirty` naming thirty groups or a reconnect loop therefore ask the server
//! at most the bucket's numbers.
//!
//! WHAT IS NOT DONE HERE: nothing is asked while the connection is not `Online` (the actor starts the task then, tells
//! it when the connection leaves `Online` ([`Fetcher::offline`]: a task that wakes for a mark in the offline catch-up of a
//! reconnect ends instead of asking, and so does one whose wait for its slot ran past a disconnect), and ends it with
//! the client); no retry loop (a failure is final for the start, or for the connection); no member of a
//! group is ever fetched one by one. The answers become events and rows in the event writer's zone, not here.
//!
//! Counts and kinds only in the log: no group id, no title, no name, no number.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use kourier_backend_api::Clock;
use kourier_model::ConvKey;
use tokio::task::JoinHandle;
use whatsapp_rust::GroupMetadataResult;

use crate::events::EventWriter;
use crate::session::{AskFailure, Session};
use crate::stage2::Stage2;
use crate::store::people;

/// At most one fetch per this long (M1).
pub(crate) const FLOOR: Duration = Duration::from_secs(30);
/// The window of [`PER_WINDOW`] (M1).
pub(crate) const WINDOW: Duration = Duration::from_secs(10 * 60);
/// At most this many fetches per [`WINDOW`] (M1).
pub(crate) const PER_WINDOW: usize = 10;
/// How long a 429 that names no delay closes the bucket.
pub(crate) const REFUSED_PAUSE: Duration = WINDOW;
/// The most groups one batch IQ asks for (upstream refuses more than 10,000 before it sends anything).
const BATCH_MAX: usize = 10_000;

/// The account's one bucket of group fetches. Pure: the caller gives the time.
#[derive(Debug, Default)]
pub(crate) struct Bucket {
    /// The times (ms) of the fetches taken inside the window, oldest first.
    taken: VecDeque<u64>,
    /// A server-named delay: no fetch before this time (ms).
    closed_until: u64,
}

impl Bucket {
    /// How long until a fetch may go, `ZERO` when one may go now. Never more than the window: a clock that was set back
    /// cannot close the bucket for longer than that.
    pub(crate) fn wait(&mut self, now_ms: u64) -> Duration {
        let window = duration_ms(WINDOW);
        while self
            .taken
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) >= window)
        {
            self.taken.pop_front();
        }
        let mut until = self.closed_until;
        if let Some(last) = self.taken.back() {
            until = until.max(last.saturating_add(duration_ms(FLOOR)));
        }
        if self.taken.len() >= PER_WINDOW
            && let Some(oldest_of_the_last) = self.taken.get(self.taken.len() - PER_WINDOW)
        {
            until = until.max(oldest_of_the_last.saturating_add(window));
        }
        let wait = until.saturating_sub(now_ms).min(window);
        Duration::from_millis(wait)
    }

    /// A fetch is made now.
    pub(crate) fn take(&mut self, now_ms: u64) {
        self.taken.push_back(now_ms);
    }

    /// The server asked for a delay: nothing is fetched before `now_ms + delay`.
    pub(crate) fn close_for(&mut self, now_ms: u64, delay: Duration) {
        self.closed_until = self
            .closed_until
            .max(now_ms.saturating_add(duration_ms(delay)));
    }
}

fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The bucket's reading of the account's clock: it never goes back (fix F7b, PROTOCOL-5). A wall clock stepped back by
/// hours would leave every entry of the bucket in the future, and the policy would sleep, look again and sleep again
/// until the clock caught up. The reading keeps the time it had: a step back moves nothing, and the time that passes
/// afterwards counts as it passes. (The account's [`Clock`] has no monotonic reading of its own and its sleeps are tokio's
/// timers, so the steadied wall reading is the monotonic one the bucket can have; a step FORWARD only ends the waits that
/// the skipped time would have covered, once.)
#[derive(Debug, Default)]
pub(crate) struct Steady {
    /// The last reading handed out.
    last: u64,
    /// How much was added to the clock's readings to keep them from going back.
    shift: u64,
}

impl Steady {
    /// The reading for the clock's `wall_ms`: never less than the one before.
    pub(crate) fn read(&mut self, wall_ms: u64) -> u64 {
        let now = wall_ms.saturating_add(self.shift);
        if now < self.last {
            self.shift = self.shift.saturating_add(self.last - now);
            self.last
        } else {
            self.last = now;
            now
        }
    }
}

/// What the policy remembers for the whole start (the actor's life), across connections.
#[derive(Debug, Default)]
pub(crate) struct Book {
    bucket: Bucket,
    /// The bucket's clock: the account's, steadied (fix F7b).
    steady: Steady,
    /// The start's two IQs are answered, or the server refused them: not asked again in this start.
    start_done: bool,
    /// Single queries may be made: the start's batch was answered. A refused batch leaves them to the next start (scout T4:
    /// no fallback to per-group IQs).
    singles: bool,
    /// The groups whose single query was made (or refused, or unreadable) in this start, with the clock's time (ms) the last
    /// one was asked at: a truncated group is not asked again, a marked one only for a mark made after that time (F17).
    tried: BTreeMap<ConvKey, u64>,
}

fn locked(book: &Mutex<Book>) -> MutexGuard<'_, Book> {
    book.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the task needs.
pub(crate) struct Ctx {
    pub(crate) session: Arc<dyn Session>,
    pub(crate) writer: EventWriter,
    pub(crate) stage2: Arc<Stage2>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) book: Arc<Mutex<Book>>,
    /// Whether the connection is `Online` (the actor's word, [`Fetcher::offline`]).
    pub(crate) online: Arc<AtomicBool>,
}

fn is_online(ctx: &Ctx) -> bool {
    ctx.online.load(Ordering::SeqCst)
}

/// The bucket's time now (ms): the account's clock, never going back (fix F7b).
fn steady_now(ctx: &Ctx) -> u64 {
    let wall = ctx.clock.now_ms();
    locked(&ctx.book).steady.read(wall)
}

/// What the task does next.
enum Job {
    /// The start's two IQs.
    Start,
    /// One single query of a group.
    One(ConvKey),
    /// Nothing is due: wait for a mark.
    Idle,
    /// The store could not be read: the task ends (the next `Online` starts it again).
    Stop,
}

/// Whether the task goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    On,
    End,
}

/// The task of one connection: runs until the connection's client goes (it is aborted) or an ask cannot reach the server.
pub(crate) async fn run(ctx: Ctx) {
    let marks = ctx.writer.marks();
    loop {
        let step = match next(&ctx).await {
            Job::Start => start(&ctx).await,
            Job::One(conv) => one(&ctx, conv).await,
            Job::Idle => {
                marks.notified().await;
                Step::On
            }
            Job::Stop => Step::End,
        };
        if step == Step::End {
            return;
        }
    }
}

/// The next job: the start's reads first; then the marks, oldest first, then the truncated groups. A truncated group is asked
/// once per start; a marked one again when its newest mark is later than its last single query (F17). None while the
/// connection is not `Online`: a mark that wakes the task in a reconnect's catch-up ends it.
async fn next(ctx: &Ctx) -> Job {
    if !is_online(ctx) {
        return Job::Stop;
    }
    let (start_done, singles, tried) = {
        let book = locked(&ctx.book);
        (book.start_done, book.singles, book.tried.clone())
    };
    if !start_done {
        return Job::Start;
    }
    if !singles {
        return Job::Idle;
    }
    let due = ctx
        .writer
        .store()
        .read(|conn| Ok((people::dirty_last(conn)?, people::truncated(conn)?)))
        .await;
    let Ok((dirty, truncated)) = due else {
        tracing::warn!(
            "the groups owed a fetch could not be read; the fetch policy stops until the next connection"
        );
        return Job::Stop;
    };
    let fresh = dirty
        .into_iter()
        .filter(|(conv, marked)| {
            tried
                .get(conv)
                .is_none_or(|asked| u64::try_from(*marked).is_ok_and(|marked| marked > *asked))
        })
        .map(|(conv, _)| conv);
    fresh
        .chain(
            truncated
                .into_iter()
                .filter(|conv| !tried.contains_key(conv)),
        )
        .next()
        .map_or(Job::Idle, Job::One)
}

/// Waits for a slot of the bucket on the account's clock (steadied: [`Steady`]) and takes it: the clock's time (ms) of
/// the slot, which is the time the ask is made at. `None` when the connection left `Online` before or while the task waited (nothing is asked then).
async fn slot(ctx: &Ctx) -> Option<u64> {
    loop {
        if !is_online(ctx) {
            return None;
        }
        // The bucket counts on the steadied reading; the ask is stamped with the clock's own (the marks it is compared with
        // are the clock's too).
        let wall = ctx.clock.now_ms();
        let wait = {
            let mut book = locked(&ctx.book);
            let now = book.steady.read(wall);
            let wait = book.bucket.wait(now);
            if wait.is_zero() {
                book.bucket.take(now);
                return Some(wall);
            }
            wait
        };
        ctx.clock.sleep(wait).await;
    }
}

/// A server that refused with a delay (or a 429 with none) closes the bucket for it.
fn honour(ctx: &Ctx, failure: AskFailure) {
    if let AskFailure::Refused { code, backoff } = failure {
        let delay = backoff.or((code == 429).then_some(REFUSED_PAUSE));
        if let Some(delay) = delay {
            let now = steady_now(ctx);
            locked(&ctx.book).bucket.close_for(now, delay);
        }
    }
}

/// The start's two IQs: the overview, then ONE batch of every group it lists.
async fn start(ctx: &Ctx) -> Step {
    let Some(asked_at) = slot(ctx).await else {
        return Step::End;
    };
    let overview = match ctx.session.participating().await {
        Ok(overview) => overview,
        Err(failure) => return start_failed(ctx, "participating", failure),
    };
    let mut asked = Vec::new();
    let mut seen = BTreeSet::new();
    for id in overview {
        if let Some(conv) = super::group_conv(&id)
            && seen.insert(conv)
        {
            asked.push(id);
        }
    }
    asked.truncate(BATCH_MAX);
    if asked.is_empty() {
        tracing::info!("the account is in no group; nothing is fetched at this start");
        finish_start(ctx);
        return Step::On;
    }
    let count = asked.len();
    if !is_online(ctx) {
        return Step::End;
    }
    let answers = match ctx.session.fetch_groups(asked).await {
        Ok(answers) => answers,
        Err(failure) => return start_failed(ctx, "batch", failure),
    };
    let answered = answers.len();
    let seen = tally(&answers);
    if !write(ctx, answers, asked_at).await {
        return Step::End;
    }
    say_fetched(count, answered, seen);
    finish_start(ctx);
    Step::On
}

/// How many answers of a batch state no whole member list (T2 T-1c, decision C4): a `Found` answer the server marked
/// incomplete (`missing_participant_identification`, fix F2: [`crate::groups::map`] states no snapshot for it) and a
/// `Truncated` one. Counts only: the run tool's report reads them from the line of [`say_fetched`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tally {
    flagged: usize,
    truncated: usize,
}

/// The [`Tally`] of a batch's answers.
pub(crate) fn tally(answers: &[GroupMetadataResult]) -> Tally {
    let mut seen = Tally::default();
    for answer in answers {
        match answer {
            GroupMetadataResult::Found(metadata)
                if metadata.has_missing_participant_identification =>
            {
                seen.flagged += 1;
            }
            GroupMetadataResult::Truncated { .. } => seen.truncated += 1,
            _ => {}
        }
    }
    seen
}

/// The ONE info line of the start's fetch. Its words and field names are read by the run tool (`xtask/src/wa_run.rs`
/// `START_FETCH_LINE`, which a test of it compares with this file): a change here is a change there. Counts only.
fn say_fetched(asked: usize, answered: usize, seen: Tally) {
    tracing::info!(
        asked,
        answered,
        flagged = seen.flagged,
        truncated = seen.truncated,
        "the groups were fetched at the start"
    );
}

/// The start's reads are done: single queries may follow.
fn finish_start(ctx: &Ctx) {
    let mut book = locked(&ctx.book);
    book.start_done = true;
    book.singles = true;
}

/// What an ask of the start that got no answer means: the connection's trouble ends the task and the next `Online` asks
/// again; a server's refusal or an unreadable answer is final for the start and leaves the single queries to the next one.
fn start_failed(ctx: &Ctx, which: &'static str, failure: AskFailure) -> Step {
    honour(ctx, failure);
    if failure.is_later() {
        tracing::warn!(
            ask = which,
            kind = failure.kind(),
            "the groups could not be asked; they are asked again at the next connection"
        );
        return Step::End;
    }
    tracing::warn!(
        ask = which,
        kind = failure.kind(),
        "the groups could not be read; they are not asked again in this start"
    );
    locked(&ctx.book).start_done = true;
    Step::On
}

/// One single query: a group marked for a fetch or truncated.
async fn one(ctx: &Ctx, conv: ConvKey) -> Step {
    let Some(asked_at) = slot(ctx).await else {
        return Step::End;
    };
    let Ok(jid) = crate::send::address::jid_of(&conv) else {
        locked(&ctx.book).tried.insert(conv, asked_at);
        return Step::On;
    };
    match ctx.session.fetch_group(jid).await {
        Ok(answer) => {
            if !write(ctx, vec![answer], asked_at).await {
                return Step::End;
            }
            tracing::debug!("a group was fetched on its own");
            locked(&ctx.book).tried.insert(conv, asked_at);
            Step::On
        }
        Err(failure) => {
            honour(ctx, failure);
            if failure.is_later() {
                tracing::warn!(
                    kind = failure.kind(),
                    "a group could not be asked; it is asked again at the next connection"
                );
                return Step::End;
            }
            tracing::warn!(
                kind = failure.kind(),
                "a group could not be read; it is not asked again in this start"
            );
            locked(&ctx.book).tried.insert(conv, asked_at);
            Step::On
        }
    }
}

/// States the answers in one zone. `asked_at` (ms) is when the ask was made: a mark newer than it is not covered by the
/// answer and stays. `false` when the zone failed (the task ends; the next `Online` asks again).
async fn write(ctx: &Ctx, answers: Vec<GroupMetadataResult>, asked_at: u64) -> bool {
    let (writer, own) = (ctx.writer.clone(), ctx.stage2.own_known());
    let asked_at = i64::try_from(asked_at).unwrap_or(i64::MAX);
    let written = tokio::task::spawn_blocking(move || {
        writer.write_groups_fetched_and_wait(answers, own, asked_at)
    })
    .await;
    if matches!(written, Ok(Ok(()))) {
        true
    } else {
        tracing::warn!(
            "the answers of a group fetch could not be stated; they are asked again at the next connection"
        );
        false
    }
}

/// The actor's side of the policy: the book of the start and the one task of the connection.
#[derive(Default)]
pub(crate) struct Fetcher {
    book: Arc<Mutex<Book>>,
    task: Option<JoinHandle<()>>,
    online: Arc<AtomicBool>,
}

impl Fetcher {
    /// The connection is `Online`: starts the task unless one still runs (it goes on by itself). A task that ended (an ask
    /// that never reached the server) is replaced: this `Online` asks again.
    pub(crate) fn start(
        &mut self,
        session: Arc<dyn Session>,
        writer: EventWriter,
        stage2: Arc<Stage2>,
        clock: Arc<dyn Clock>,
    ) {
        self.online.store(true, Ordering::SeqCst);
        if self.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        self.task = Some(tokio::spawn(run(Ctx {
            session,
            writer,
            stage2,
            clock,
            book: self.book.clone(),
            online: self.online.clone(),
        })));
    }

    /// The connection left `Online` (any other state): the task asks nothing until the next [`start`](Self::start).
    pub(crate) fn offline(&self) {
        self.online.store(false, Ordering::SeqCst);
    }

    /// The client goes: the task ends at once (it holds the client's session). The book stays: the bucket and what the
    /// start did are the actor's.
    pub(crate) async fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests;
