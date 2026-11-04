//! The stop state of a message's attempt (upload progress: the X on an upload).
//!
//! An attempt is one `Command::Transmit`. Everything it does before `outgoing::build` returns
//! (the uploads, a quote's thumbnail) sends nothing to anyone; once it returned, the message (or,
//! in a group, the sender-key distribution) is on its way and nothing can take it back. That
//! one point is what [`Stop`] guards: [`Stop::request`] (an `AbortTransmit`) and [`Stop::close`]
//! (the attempt passing the point) are plain functions on a single-threaded actor, so exactly
//! one of them wins, and the answer to the abort is decided by which one it was, at once, with
//! no `.await` between the look at the state and the change of it:
//!
//! - the abort came first: [`AbortAnswer::Stopping`], and the attempt ends before anything goes
//!   out (its work is dropped where it stands, the `Transmit` answers `Failed`);
//! - the attempt passed first (or there is no such attempt): [`AbortAnswer::TooLate`].
//!
//! The attempt is never killed from outside (a dropped reply would read as "may have reached the
//! server" to the core): it notices the stop and answers its own `Transmit`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use kourier_backend_api::AbortAnswer;
use kourier_model::{BackendError, ErrorKind, LocalMsgId};
use tokio::sync::Notify;

/// Where an attempt stands relative to its point of no return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopState {
    /// Before the point: an abort is accepted.
    Open,
    /// An abort was accepted: nothing of the attempt goes out any more.
    Requested,
    /// Past the point (or never had one): an abort is too late.
    Closed,
}

/// One attempt's stop state.
pub(crate) struct Stop {
    state: Cell<StopState>,
    /// Woken when the state becomes [`StopState::Requested`].
    requested: Notify,
}

impl Stop {
    /// An attempt that can still be stopped (a new message: its uploads come first).
    pub(crate) fn open() -> Self {
        Self::with(StopState::Open)
    }

    /// An attempt that cannot be stopped: an edit, a delete, a reaction have no upload phase
    /// worth stopping, and their message may have reached someone already.
    pub(crate) fn closed() -> Self {
        Self::with(StopState::Closed)
    }

    fn with(state: StopState) -> Self {
        Self {
            state: Cell::new(state),
            requested: Notify::new(),
        }
    }

    /// `AbortTransmit`: stops the attempt if it is before its point of no return. Repeating it
    /// repeats the answer. No `.await` anywhere: the look and the change are one step.
    pub(crate) fn request(&self) -> AbortAnswer {
        match self.state.get() {
            StopState::Open => {
                self.state.set(StopState::Requested);
                self.requested.notify_waiters();
                AbortAnswer::Stopping
            }
            StopState::Requested => AbortAnswer::Stopping,
            StopState::Closed => AbortAnswer::TooLate,
        }
    }

    /// The attempt is past its point of no return: from now on an abort is too late. `false`
    /// when an abort was accepted first: the attempt must send nothing and end.
    pub(crate) fn close(&self) -> bool {
        match self.state.get() {
            StopState::Open => {
                self.state.set(StopState::Closed);
                true
            }
            StopState::Requested => false,
            StopState::Closed => true,
        }
    }

    /// An abort was accepted.
    pub(crate) fn is_requested(&self) -> bool {
        self.state.get() == StopState::Requested
    }

    /// Completes when an abort is accepted (at once if it was).
    pub(crate) async fn requested(&self) {
        loop {
            // Registered before the look, so a `request` between the two is not missed.
            let woken = self.requested.notified();
            if self.is_requested() {
                return;
            }
            woken.await;
        }
    }

    /// Runs `work` until it ends or an abort is accepted; the work is dropped where it stands in
    /// the second case.
    pub(crate) async fn until_stopped<F, R>(&self, work: F) -> Result<R, BackendError>
    where
        F: Future<Output = Result<R, BackendError>>,
    {
        tokio::select! {
            biased;
            () = self.requested() => Err(stopped()),
            done = work => done,
        }
    }
}

/// What a stopped attempt fails with. The core keys on the cancel it recorded before it asked,
/// not on this: it takes the message back (or, if that record is missing, shows it failed, never
/// retried: not a transient kind).
pub(crate) fn stopped() -> BackendError {
    BackendError::new(
        ErrorKind::Internal,
        "stopped by the user before anything was sent",
    )
}

/// The attempts of an account that are running, by message.
#[derive(Default)]
pub(crate) struct Transmits {
    attempts: RefCell<HashMap<LocalMsgId, Rc<Stop>>>,
}

impl Transmits {
    /// Registers the attempt of `local` with `stop`, synchronously when its `Transmit` arrives
    /// (an `AbortTransmit` right behind it must find it). It is forgotten when the returned
    /// guard goes: the task answered, or was dropped.
    pub(crate) fn register(self: &Rc<Self>, local: LocalMsgId, stop: Stop) -> Registered {
        let stop = Rc::new(stop);
        self.attempts.borrow_mut().insert(local, stop.clone());
        Registered {
            transmits: self.clone(),
            local,
            stop,
        }
    }

    /// `AbortTransmit` for `local`: the attempt decides; none running is too late.
    pub(crate) fn abort(&self, local: LocalMsgId) -> AbortAnswer {
        match self.attempts.borrow().get(&local) {
            Some(stop) => stop.request(),
            None => AbortAnswer::TooLate,
        }
    }

    /// How many attempts are registered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.attempts.borrow().len()
    }
}

/// An attempt's place in [`Transmits`], until it ends.
pub(crate) struct Registered {
    transmits: Rc<Transmits>,
    local: LocalMsgId,
    stop: Rc<Stop>,
}

impl Registered {
    /// The attempt's stop state.
    pub(crate) fn stop(&self) -> &Stop {
        &self.stop
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut attempts = self.transmits.attempts.borrow_mut();
        // A newer attempt of the same message (never at once with this one) keeps its entry.
        if attempts
            .get(&self.local)
            .is_some_and(|stop| Rc::ptr_eq(stop, &self.stop))
        {
            attempts.remove(&self.local);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The abort and the point of no return are one race with one winner: whichever comes
    /// first decides, and what it decided is what the other sees.
    #[test]
    fn the_abort_and_the_point_of_no_return_have_one_winner() {
        let stop = Stop::open();
        assert_eq!(stop.request(), AbortAnswer::Stopping);
        assert_eq!(stop.request(), AbortAnswer::Stopping, "repeating repeats");
        assert!(!stop.close(), "the attempt must not pass after a stop");
        assert!(stop.is_requested());

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
    }

    /// An unknown message is too late; a running one answers from its own state; the entry
    /// goes with the attempt's guard and a newer attempt's entry is not removed by an older
    /// guard.
    #[test]
    fn attempts_are_found_by_message_and_forgotten_when_they_end() {
        let attempts = Rc::new(Transmits::default());
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
        assert!(!second.stop().is_requested());
        drop(second);
        drop(other);
        assert_eq!(attempts.len(), 0);
    }

    /// Work in `until_stopped` ends with the stop, at once, wherever it waits; work that ends
    /// first returns its own result.
    #[tokio::test(flavor = "current_thread")]
    async fn work_ends_with_the_stop() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let stop = Rc::new(Stop::open());
                let waiting = {
                    let stop = stop.clone();
                    tokio::task::spawn_local(async move {
                        stop.until_stopped(std::future::pending::<Result<(), BackendError>>())
                            .await
                    })
                };
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(!waiting.is_finished(), "it waits while nothing asks");
                assert_eq!(stop.request(), AbortAnswer::Stopping);
                let ended = tokio::time::timeout(Duration::from_secs(1), waiting)
                    .await
                    .expect("ends with the stop")
                    .unwrap();
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
            })
            .await;
    }
}
