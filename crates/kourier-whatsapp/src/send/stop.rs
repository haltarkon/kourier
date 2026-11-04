//! The stop state of a message's attempt (W5b, decision G2; `.claude/rules/upload-progress.md`: the X on an
//! upload).
//!
//! An attempt is one `Command::Transmit`. Everything it does before its frame is written (reading the core's file,
//! the re-encode, the upload to the CDN) sends nothing to anyone; once the frame is written the message is on its
//! way and nothing can take it back. That one point is what [`Stop`] guards: [`Stop::request`] (an
//! `AbortTransmit`) and [`Stop::close`] (the attempt passing the point, right before `write_once`, after the hold's
//! re-check) are ONE `compare_exchange` each on the same atomic, so exactly one of them wins, whichever thread runs
//! them, and the answer to the abort is decided by which one it was, at once, with no `.await` between the look at
//! the state and the change of it:
//!
//! - the abort came first: [`AbortAnswer::Stopping`], and the attempt ends before anything goes out (it sees the
//!   stop at its next await or read and answers its `Transmit` `Failed` with [`stopped`]);
//! - the attempt passed first (or there is no such attempt): [`AbortAnswer::TooLate`].
//!
//! Unlike Signal's (`kourier-signal/src/send/stop.rs`, a `Cell` on a single-threaded actor) this one is
//! `Send + Sync`: the attempts are tasks of a `JoinSet` with `Send` futures, and the upload's blocking reader looks at
//! it from a thread of the blocking pool.
//!
//! [`Transmits`] holds the attempts under way by message. It is filled SYNCHRONOUSLY in `Actor::transmit` before
//! the attempt is spawned (an `AbortTransmit` right behind the `Transmit` must find it) and the attempt's
//! [`Registered`] guard removes the entry when the attempt ends or its task is dropped. Only a new message with
//! files gets [`Stop::open`]; a text, an edit (a caption edit too), a delete and a reaction get [`Stop::closed`]: they
//! have no upload to report, so the core never offers their X.
//!
//! The attempt is never killed from outside (a dropped reply would read as "may have reached the server" to the
//! core): it notices the stop and answers its own `Transmit`.
//!
//! A QUIT of the app (fix F16, the builder's Q-B16) is the same race with a different answer. [`Transmits::quit_all`]
//! moves every attempt that is still before its point of no return to a fourth state, QUITTING, by the same
//! `compare_exchange(OPEN, ..)` as the abort and the close: exactly one of the three wins. The attempt that lost to the
//! quit sees it at its next await or read (the blocking reads fail through [`Halt`](super::upload::Halt)), writes
//! nothing and answers [`quitting`] (`Transient`: the core sends the message again at the next start; it is NOT
//! [`stopped`], which the core shows as failed for good); the attempt that passed first (CLOSED: a frame about to be
//! written or in the air, and every text, edit, delete and reaction) is waited for by the quit as before (W5a D19).
//! An X pressed meanwhile is still `Stopping` (nothing goes out) and turns the state into REQUESTED, so the attempt
//! ends as a stop (its upload record is deleted, fix F13).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kourier_backend_api::AbortAnswer;
use kourier_model::{BackendError, ErrorKind, LocalMsgId, MessageId};
use tokio::sync::Notify;

/// Before the point of no return: an abort is accepted.
const OPEN: u8 = 0;
/// An abort was accepted: nothing of the attempt goes out any more.
const REQUESTED: u8 = 1;
/// Past the point (or never had one): an abort is too late.
const CLOSED: u8 = 2;
/// The app is quitting and the attempt is still before its point (fix F16): nothing of it goes out; it ends
/// `Transient` ([`quitting`]).
const QUITTING: u8 = 3;

struct Inner {
    state: AtomicU8,
    /// Woken when the state becomes [`REQUESTED`] or [`QUITTING`].
    requested: Notify,
}

/// One attempt's stop state (cheap to clone: every clone is the same state).
#[derive(Clone)]
pub(crate) struct Stop(Arc<Inner>);

impl Stop {
    /// An attempt that can still be stopped (a new message with files: its upload comes first).
    pub(crate) fn open() -> Self {
        Self::with(OPEN)
    }

    /// An attempt that cannot be stopped: a text, an edit, a delete, a reaction have no upload phase worth
    /// stopping, and an edit's or a delete's original may have reached someone already.
    pub(crate) fn closed() -> Self {
        Self::with(CLOSED)
    }

    fn with(state: u8) -> Self {
        Self(Arc::new(Inner {
            state: AtomicU8::new(state),
            requested: Notify::new(),
        }))
    }

    /// `AbortTransmit`: stops the attempt if it is before its point of no return. Repeating it repeats the answer.
    /// One `compare_exchange`: the look and the change are one step. An attempt the quit already took (QUITTING) has
    /// not passed its point either: the abort is `Stopping` and the state becomes REQUESTED, so the attempt ends as
    /// the person's stop.
    pub(crate) fn request(&self) -> AbortAnswer {
        let state = &self.0.state;
        let won = state
            .compare_exchange(OPEN, REQUESTED, Ordering::AcqRel, Ordering::Acquire)
            .or_else(|seen| {
                if seen == QUITTING {
                    state.compare_exchange(QUITTING, REQUESTED, Ordering::AcqRel, Ordering::Acquire)
                } else {
                    Err(seen)
                }
            });
        match won {
            Ok(_) => {
                self.0.requested.notify_waiters();
                AbortAnswer::Stopping
            }
            Err(REQUESTED) => AbortAnswer::Stopping,
            Err(_) => AbortAnswer::TooLate,
        }
    }

    /// The app is quitting (fix F16): an attempt before its point of no return ends without a frame, `Transient`.
    /// `true` = the attempt ends by itself (the quit took it, or an abort had); `false` = it is past its point (or
    /// has none) and the quit waits for it. One `compare_exchange`, the same atomic as [`request`](Self::request)
    /// and [`close`](Self::close).
    pub(crate) fn quit(&self) -> bool {
        match self
            .0
            .state
            .compare_exchange(OPEN, QUITTING, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                self.0.requested.notify_waiters();
                true
            }
            Err(QUITTING | REQUESTED) => true,
            Err(_) => false,
        }
    }

    /// The attempt is past its point of no return: from now on an abort is too late. `false` when an abort was
    /// accepted first, or the quit took the attempt: the attempt must send nothing and end.
    pub(crate) fn close(&self) -> bool {
        match self
            .0
            .state
            .compare_exchange(OPEN, CLOSED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(CLOSED) => true,
            Err(_) => false,
        }
    }

    /// An abort was accepted.
    pub(crate) fn is_requested(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == REQUESTED
    }

    /// The quit took the attempt before its point of no return (fix F16).
    pub(crate) fn is_quitting(&self) -> bool {
        self.0.state.load(Ordering::Acquire) == QUITTING
    }

    /// Whether the attempt must send nothing and end: an abort was accepted, or the quit took it. What the blocking
    /// reads look at.
    pub(crate) fn is_ended(&self) -> bool {
        matches!(self.0.state.load(Ordering::Acquire), REQUESTED | QUITTING)
    }

    /// What the attempt that [`is_ended`](Self::is_ended) answers: [`stopped`] for the person's stop, [`quitting`]
    /// for the quit.
    pub(crate) fn ended_error(&self) -> BackendError {
        if self.is_quitting() {
            quitting()
        } else {
            stopped()
        }
    }

    /// Completes when an abort is accepted or the quit takes the attempt (at once if it did).
    pub(crate) async fn ended(&self) {
        loop {
            // Created before the look: `notify_waiters` wakes a `Notified` from its creation on, so a `request`
            // between the two is not missed.
            let woken = self.0.requested.notified();
            if self.is_ended() {
                return;
            }
            woken.await;
        }
    }

    /// Runs `work` until it ends, an abort is accepted or the quit takes the attempt; the work is dropped where it
    /// stands in the other two cases (the answer is [`stopped`], or [`quitting`] for the quit).
    pub(crate) async fn until_stopped<F, R>(&self, work: F) -> Result<R, BackendError>
    where
        F: Future<Output = Result<R, BackendError>>,
    {
        tokio::select! {
            biased;
            () = self.ended() => Err(self.ended_error()),
            done = work => done,
        }
    }

    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What a stopped attempt fails with (Signal's words and kind). The core keys on the cancel it recorded before it
/// asked, not on this: it takes the message back (or, if that record is missing, shows it failed, never retried:
/// not a transient kind).
pub(crate) fn stopped() -> BackendError {
    BackendError::new(
        ErrorKind::Internal,
        "stopped by the user before anything was sent",
    )
}

/// What an attempt the app's quit took answers (fix F16): `Transient` with no wait. Nothing went out, and the core
/// sends the message again at the next start (a dropped reply reads the same); NOT [`stopped`], which the core shows
/// as failed for good.
pub(crate) fn quitting() -> BackendError {
    BackendError::transient("the app was closing before anything was sent")
}

/// The attempts of an account that are running, by message. The lock is never held across an await.
#[derive(Clone, Default)]
pub(crate) struct Transmits {
    attempts: Arc<Mutex<HashMap<LocalMsgId, Stop>>>,
    /// The count of [`Self::register_change`]'s keys.
    changes: Arc<AtomicU64>,
}

impl Transmits {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<LocalMsgId, Stop>> {
        self.attempts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers the attempt of `local` with `stop`, synchronously when its `Transmit` arrives (an `AbortTransmit`
    /// right behind it must find it). It is forgotten when the returned guard goes: the attempt ended, or its task
    /// was dropped.
    pub(crate) fn register(&self, local: LocalMsgId, stop: Stop) -> Registered {
        self.map().insert(local, stop.clone());
        Registered {
            transmits: self.clone(),
            local,
            stop,
        }
    }

    /// Registers the attempt of a conversation change that writes a frame (the 1:1 timer set, W6a2 B7b), which has no
    /// message row of the core's: its key comes from a counter and is NEGATIVE, so it can never be the key of a message
    /// (the core's ids are rowids from 1) and no `AbortTransmit` finds it. It is [`Stop::open`], so the app's quit takes
    /// it before its frame like any other attempt ([`Self::quit_all`]) and waits for one past its point.
    pub(crate) fn register_change(&self) -> Registered {
        let n = self.changes.fetch_add(1, Ordering::Relaxed);
        let key = i64::try_from(n).unwrap_or(i64::MAX).saturating_add(1);
        self.register(MessageId(-key), Stop::open())
    }

    /// `AbortTransmit` for `local`: the attempt decides; none running is too late.
    pub(crate) fn abort(&self, local: LocalMsgId) -> AbortAnswer {
        self.map()
            .get(&local)
            .map_or(AbortAnswer::TooLate, Stop::request)
    }

    /// The app is quitting (fix F16): every attempt before its point of no return is told to end without a frame
    /// ([`Stop::quit`]). Returns `(ended, kept)`: the attempts that end by themselves, and the ones the quit still
    /// waits for (past their point, or never stoppable: texts, edits, deletes, reactions).
    pub(crate) fn quit_all(&self) -> (usize, usize) {
        // The stops are collected first: the lock is not held while they are woken.
        let stops: Vec<Stop> = self.map().values().cloned().collect();
        let ended = stops.iter().filter(|stop| stop.quit()).count();
        (ended, stops.len() - ended)
    }

    /// How many attempts are registered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map().len()
    }
}

/// An attempt's place in [`Transmits`], until it ends.
pub(crate) struct Registered {
    transmits: Transmits,
    local: LocalMsgId,
    stop: Stop,
}

impl Registered {
    /// The attempt's stop state.
    pub(crate) fn stop(&self) -> &Stop {
        &self.stop
    }

    /// The message row the attempt is for (its reports name it).
    pub(crate) fn local(&self) -> LocalMsgId {
        self.local
    }

    /// A guard in a map of its own (an attempt no `AbortTransmit` can reach: the unit tests of the attempt).
    #[cfg(test)]
    pub(crate) fn alone(stop: Stop) -> Self {
        Transmits::default().register(kourier_model::MessageId(0), stop)
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut attempts = self.transmits.map();
        // A newer attempt of the same message (a restart's, never at once with this one) keeps its entry.
        if attempts
            .get(&self.local)
            .is_some_and(|stop| stop.same(&self.stop))
        {
            attempts.remove(&self.local);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The abort and the point of no return are one race with one winner: whichever comes first decides, and what
    /// it decided is what the other sees; a repeated abort repeats its answer; a closed stop is too late. Mutants:
    /// `close` that stores instead of `compare_exchange`; a repeated request answering `TooLate`.
    #[test]
    fn one_of_request_and_close_wins() {
        let stop = Stop::open();
        assert_eq!(stop.request(), AbortAnswer::Stopping);
        assert_eq!(stop.request(), AbortAnswer::Stopping, "repeating repeats");
        assert!(!stop.close(), "the attempt must not pass after a stop");
        assert!(stop.is_requested(), "and the stop stands");

        let stop = Stop::open();
        assert!(stop.close(), "no abort yet: the attempt passes");
        assert_eq!(
            stop.request(),
            AbortAnswer::TooLate,
            "an abort after the point changes nothing"
        );
        assert!(!stop.is_requested(), "and does not stop the attempt");
        assert!(stop.close(), "passing again is still passing");

        let never = Stop::closed();
        assert_eq!(never.request(), AbortAnswer::TooLate);
        assert!(never.close());
        assert!(!never.is_requested());
    }

    /// The same race on two threads, 10,000 times: exactly one side wins each time.
    #[test]
    fn the_race_on_two_threads_has_one_winner_each_time() {
        for _ in 0..10_000 {
            let stop = Stop::open();
            let start = std::sync::Barrier::new(2);
            let (stopping, passed) = std::thread::scope(|s| {
                let abort = s.spawn(|| {
                    start.wait();
                    stop.request() == AbortAnswer::Stopping
                });
                start.wait();
                let passed = stop.close();
                (abort.join().expect("no panic"), passed)
            });
            assert_ne!(stopping, passed, "exactly one winner");
            assert_eq!(stop.is_requested(), stopping);
        }
    }

    /// Fix F16: the app's quit, the abort and the point of no return are one race with ONE winner. The quit takes an
    /// open attempt (its close fails, `is_quitting`), waits for a closed one (the quit answers `false`), joins an
    /// attempt the person already stopped; an abort that comes after the quit is `Stopping` and turns the state into
    /// the person's stop (so the attempt ends as a stop, its record deleted); a repeated quit changes nothing.
    /// Mutants: `close` that lets a quitting attempt pass; the quit that takes a closed attempt; the abort that is
    /// `TooLate` after the quit.
    #[test]
    fn one_of_quit_request_and_close_wins() {
        let stop = Stop::open();
        assert!(stop.quit(), "an open attempt is taken");
        assert!(stop.is_quitting() && stop.is_ended());
        assert!(!stop.is_requested());
        assert!(stop.quit(), "repeating repeats");
        assert!(!stop.close(), "a quitting attempt must not pass its point");
        assert!(stop.is_quitting(), "and the quit stands");
        assert_eq!(
            stop.ended_error().kind,
            ErrorKind::Transient { retry_after: None }
        );

        // The person's X after the quit: nothing goes out either way, so it is `Stopping`, and the attempt ends as a stop.
        assert_eq!(stop.request(), AbortAnswer::Stopping);
        assert!(stop.is_requested() && !stop.is_quitting());
        assert_eq!(stop.ended_error().kind, ErrorKind::Internal);
        assert!(!stop.close());
        assert!(
            stop.quit(),
            "an attempt the person stopped is ending already"
        );
        assert!(stop.is_requested(), "the quit does not take a stop back");

        // The attempt passed first: the quit leaves it (the quit waits for it) and changes nothing.
        let passed = Stop::open();
        assert!(passed.close());
        assert!(!passed.quit(), "past the point of no return: waited for");
        assert!(!passed.is_ended());
        assert!(passed.close(), "and it still passes");
        let never = Stop::closed();
        assert!(
            !never.quit(),
            "a text, an edit, a delete, a reaction: waited for"
        );
        assert!(never.close());
    }

    /// The same race on two threads, 10,000 times: the quit and the close have exactly one winner each time, and what
    /// the loser sees is what the winner decided. Mutant: `close` that stores instead of `compare_exchange`.
    #[test]
    fn the_quit_and_the_close_have_one_winner_on_two_threads() {
        for _ in 0..10_000 {
            let stop = Stop::open();
            let start = std::sync::Barrier::new(2);
            let (taken, passed) = std::thread::scope(|s| {
                let quit = s.spawn(|| {
                    start.wait();
                    stop.quit()
                });
                start.wait();
                let passed = stop.close();
                (quit.join().expect("no panic"), passed)
            });
            assert_ne!(taken, passed, "exactly one winner");
            assert_eq!(stop.is_quitting(), taken);
        }
    }

    /// Fix F16: the quit tells every attempt before its point to end and reports who it will wait for: an open one
    /// ends (and its work in `until_stopped` ends with `Transient`, not with the person's `Internal`), a closed one
    /// and an attempt past its point are kept; the numbers are `(ended, kept)`. Mutant: `quit_all` that takes every
    /// attempt (the closed ones too).
    #[tokio::test]
    async fn the_quit_ends_the_open_attempts_and_waits_for_the_closed_ones() {
        let attempts = Transmits::default();
        let ids = |n| kourier_model::MessageId(n);
        let open = attempts.register(ids(1), Stop::open());
        let text = attempts.register(ids(2), Stop::closed());
        let past = attempts.register(ids(3), Stop::open());
        assert!(past.stop().close());
        let stopped_by_the_person = attempts.register(ids(4), Stop::open());
        assert_eq!(attempts.abort(ids(4)), AbortAnswer::Stopping);

        let waiting = {
            let stop = open.stop().clone();
            tokio::spawn(async move {
                stop.until_stopped(std::future::pending::<Result<(), BackendError>>())
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        // The open attempt and the one the person stopped end by themselves; the text and the passed one are waited for.
        assert_eq!(attempts.quit_all(), (2, 2));
        let ended = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the quit ends the attempt's work")
            .expect("no panic")
            .unwrap_err();
        assert_eq!(ended.kind, ErrorKind::Transient { retry_after: None });
        assert!(open.stop().is_quitting());
        assert!(!text.stop().is_ended() && !past.stop().is_ended());
        assert!(
            stopped_by_the_person.stop().is_requested(),
            "a stop stays a stop"
        );
        assert_eq!(attempts.quit_all(), (2, 2), "a second quit changes nothing");
    }

    /// An unknown message is too late; a running one answers from its own state; the entry goes with the attempt's
    /// guard and a newer attempt's entry is not removed by an older guard. Mutant: the guard removes by local alone.
    #[test]
    fn attempts_are_found_by_message_and_forgotten_when_they_end() {
        let attempts = Transmits::default();
        let (a, b) = (kourier_model::MessageId(1), kourier_model::MessageId(2));
        assert_eq!(attempts.abort(a), AbortAnswer::TooLate, "unknown");
        let first = attempts.register(a, Stop::open());
        let other = attempts.register(b, Stop::closed());
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts.abort(b), AbortAnswer::TooLate, "closed");
        assert_eq!(attempts.abort(a), AbortAnswer::Stopping);
        assert!(first.stop().is_requested());
        // The same message again (a restart's attempt replacing a stale entry).
        let second = attempts.register(a, Stop::open());
        drop(first);
        assert_eq!(attempts.len(), 2, "the older guard leaves the newer entry");
        assert_eq!(
            attempts.abort(a),
            AbortAnswer::Stopping,
            "the newer attempt is found"
        );
        assert!(second.stop().is_requested());
        drop(second);
        drop(other);
        assert_eq!(attempts.len(), 0);
        assert_eq!(attempts.abort(a), AbortAnswer::TooLate, "gone");
    }

    /// Work in `until_stopped` ends with the stop, at once, wherever it waits, also when the stop comes from
    /// another thread; work that ends first returns its own result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn work_ends_with_the_stop() {
        let stop = Stop::open();
        let waiting = {
            let stop = stop.clone();
            tokio::spawn(async move {
                stop.until_stopped(std::future::pending::<Result<(), BackendError>>())
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished(), "it waits while nothing asks");
        let from = stop.clone();
        let answer = std::thread::spawn(move || from.request())
            .join()
            .expect("no panic");
        assert_eq!(answer, AbortAnswer::Stopping);
        let ended = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("ends with the stop")
            .expect("no panic");
        assert_eq!(ended.unwrap_err().kind, ErrorKind::Internal);

        let finished = Stop::open()
            .until_stopped(async { Ok::<_, BackendError>(5) })
            .await;
        assert_eq!(finished.unwrap(), 5);
        // A stop that came before the work started ends it before it runs.
        let already = Stop::open();
        already.request();
        assert!(
            already
                .until_stopped(async { Ok::<_, BackendError>(5) })
                .await
                .is_err()
        );
    }

    /// W6a2 B7b: the attempt of a conversation change (the 1:1 timer set) has no message row. Its key can never be a
    /// message's (negative, and every one different), no `AbortTransmit` of any message finds it, the guard forgets it
    /// when the attempt ends, and the quit takes it before its frame like any open attempt. Mutants: the same key for
    /// every change; a positive key; `Stop::closed()` for a change.
    #[test]
    fn a_conversation_changes_attempt_has_a_key_no_message_has_and_the_quit_takes_it() {
        let transmits = Transmits::default();
        let first = transmits.register_change();
        let second = transmits.register_change();
        assert!(
            first.local().0 < 0 && second.local().0 < 0,
            "never a message row"
        );
        assert_ne!(first.local(), second.local(), "one entry each");
        assert_eq!(transmits.len(), 2);
        assert_eq!(
            transmits.abort(MessageId(1)),
            AbortAnswer::TooLate,
            "a message's abort finds no change"
        );
        assert_eq!(
            transmits.quit_all(),
            (2, 0),
            "both are still before their point of no return"
        );
        assert!(second.stop().is_quitting());
        drop(first);
        assert_eq!(transmits.len(), 1, "the guard forgets its entry");
        drop(second);
        assert_eq!(transmits.len(), 0);
    }
}
