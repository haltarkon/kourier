//! WHAT THE ACTOR AND ITS SOCKET SHARE OF THE QUEUE (TH6a; the skeleton is S4's, S5a's socket uses it and S6's `Transmit`
//! attaches to it): one thread, so plain cells. The rows themselves are the file's; this is only what runs now, and the
//! replies that wait on it ([`super::attempt`]).
//!
//! THE COOLING (S5b, decision R2): a row the server will not take must neither stop the account nor churn its socket
//! nor hold later rows. A socket that ended by a missing ack or by the server's close with frames of the running row
//! unanswered is a FAILED RUN of that row (`conn::state::Outcome::is_failed_run`; a lost network is none). The first is
//! free, a server restart or a one-off close is common; from the second the row cools [`cooling_after`]: `next_send`
//! skips it, later rows go, and the socket stays up and wakes when it may run again, so `close::Backoff` sees a socket
//! that worked and the account never reaches `NeedsUserAction` through a row. The count is durable
//! (`queue::count_failed_run`), the time here is not: a restart loses the cooling and not the count. A job gives up at
//! [`JOB_GIVES_UP_AT`]; a `transmit` row has no give-up of its own (the core's 24 h budget, then the 30-day sweep).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::Duration;

use kourier_model::{BackendError, ErrorKind};
use tokio::sync::Notify;

use super::attempt::Attempt;

/// The cooling after a row's second failed run; it doubles with each further one.
pub(crate) const COOL_FIRST: Duration = Duration::from_secs(2 * 60);
/// The longest cooling.
pub(crate) const COOL_MOST: Duration = Duration::from_secs(15 * 60);
/// A job is given up at this many failed runs: with [`cooling_after`] about 2.5 hours.
pub(crate) const JOB_GIVES_UP_AT: u32 = 12;

/// How long a row cools after its `failed_runs`-th failed run; `None` for the first, which is free.
pub(crate) fn cooling_after(failed_runs: u32) -> Option<Duration> {
    let doublings = failed_runs.checked_sub(2)?;
    let doubled = 1_u32
        .checked_shl(doublings)
        .and_then(|factor| COOL_FIRST.checked_mul(factor))
        .unwrap_or(COOL_MOST);
    Some(doubled.min(COOL_MOST))
}

/// The queue's state in memory, lost with the actor. A restart loses nothing durable: a `Transmit` attaches again.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    /// The `Transmit`s that wait on a row, each with its reply: their tasks are `next_send`'s `attached`.
    attached: RefCell<Vec<Attempt>>,
    /// The tasks that are cooling and until when (ms of the actor's clock).
    cooling: RefCell<BTreeMap<i64, u64>>,
    /// Wakes the socket's loop: a row may run (a commit that made a row, or a `Transmit` attached).
    kick: Notify,
    /// Whether a socket stands past the chat server's login-ack.
    live: Cell<bool>,
}

impl Shared {
    /// A `Transmit` waits on its row. The attempt that waited on that row until now, if any: it is the caller's to answer.
    pub(crate) fn attach(&self, attempt: Attempt) -> Option<Attempt> {
        let older = self.detach(attempt.task());
        self.attached.borrow_mut().push(attempt);
        older
    }

    /// The attempt on `task`, no longer attached: it is the caller's to answer.
    pub(crate) fn detach(&self, task: i64) -> Option<Attempt> {
        let mut attached = self.attached.borrow_mut();
        let at = attached.iter().position(|a| a.task() == task)?;
        Some(attached.remove(at))
    }

    /// Every attempt, no longer attached: they are the caller's to answer.
    pub(crate) fn detach_all(&self) -> Vec<Attempt> {
        std::mem::take(&mut self.attached.borrow_mut())
    }

    /// The tasks a `Transmit` is attached to, in the order they came.
    pub(crate) fn attached(&self) -> Vec<i64> {
        self.attached.borrow().iter().map(Attempt::task).collect()
    }

    /// Whom the attempt on `task` asks for (`None`: everyone, or no attempt).
    pub(crate) fn only_of(&self, task: i64) -> Option<Vec<String>> {
        let attached = self.attached.borrow();
        let attempt = attached.iter().find(|a| a.task() == task)?;
        attempt.only().map(<[String]>::to_vec)
    }

    /// Task `task` does not run before `until_ms`.
    pub(crate) fn cool(&self, task: i64, until_ms: u64) {
        self.cooling.borrow_mut().insert(task, until_ms);
    }

    /// The tasks still cooling at `now_ms`: `next_send`'s `cooling`. What has thawed is forgotten.
    pub(crate) fn cooling(&self, now_ms: u64) -> Vec<i64> {
        let mut cooling = self.cooling.borrow_mut();
        cooling.retain(|_, until| *until > now_ms);
        cooling.keys().copied().collect()
    }

    /// When the next row thaws, after `now_ms`: the socket wakes then, so the row runs again without a new socket. What
    /// has thawed is forgotten, so a row that thawed behind a running one wakes nothing again.
    pub(crate) fn next_thaw(&self, now_ms: u64) -> Option<u64> {
        let mut cooling = self.cooling.borrow_mut();
        cooling.retain(|_, until| *until > now_ms);
        cooling.values().copied().min()
    }

    /// What a `Transmit` of `task` is answered while the row cools: it writes no frame and the core comes back when the
    /// time left is over. `None`: the row is not cooling.
    pub(crate) fn cooling_answer(&self, task: i64, now_ms: u64) -> Option<BackendError> {
        let until = *self.cooling.borrow().get(&task)?;
        (until > now_ms).then(|| {
            BackendError::new(
                ErrorKind::Transient {
                    retry_after: Some(Duration::from_millis(until - now_ms)),
                },
                "the row the server did not take is cooling",
            )
        })
    }

    /// A row may run: wakes the socket's loop, or its next look when it is not waiting.
    pub(crate) fn kick(&self) {
        self.kick.notify_one();
    }

    /// Waits for [`Shared::kick`].
    pub(crate) async fn kicked(&self) {
        self.kick.notified().await;
    }

    /// Whether a socket stands past the login-ack.
    pub(crate) fn live(&self) -> bool {
        self.live.get()
    }

    /// A socket passed the login-ack, or is gone.
    pub(crate) fn set_live(&self, live: bool) {
        self.live.set(live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An attempt on `task` and the far end of its reply.
    fn attempt(
        task: i64,
        only: Option<Vec<String>>,
    ) -> (
        Attempt,
        tokio::sync::oneshot::Receiver<kourier_backend_api::TransmitOutcome>,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        (Attempt::new(task, only, reply), answer)
    }

    /// One attempt per row: a newer one hands the older back to be answered; detached by its id; `only` is the attempt's.
    #[test]
    fn a_row_holds_one_attempt_and_a_newer_one_hands_the_older_back() {
        let shared = Shared::default();
        assert!(shared.attach(attempt(3, None).0).is_none());
        assert!(
            shared
                .attach(attempt(1, Some(vec!["PEER0001".to_owned()])).0)
                .is_none()
        );
        let older = shared.attach(attempt(3, None).0).expect("the older");
        assert_eq!(older.task(), 3);
        assert_eq!(shared.attached(), [1, 3]);
        assert_eq!(shared.only_of(1), Some(vec!["PEER0001".to_owned()]));
        assert_eq!(shared.only_of(3), None);
        assert_eq!(shared.only_of(9), None);
        assert_eq!(shared.detach(3).map(|a| a.task()), Some(3));
        assert!(shared.detach(3).is_none());
        assert_eq!(shared.attached(), [1]);
        assert_eq!(shared.detach_all().len(), 1);
        assert_eq!(shared.attached(), [0_i64; 0]);
    }

    #[test]
    fn no_socket_is_live_until_one_says_so() {
        let shared = Shared::default();
        assert!(!shared.live());
        shared.set_live(true);
        assert!(shared.live());
        shared.set_live(false);
        assert!(!shared.live());
    }

    /// The table of R2, written out: the first failed run is free; 2 minutes, doubling, capped at 15; far past the end of
    /// the doublings it stays at the cap. A job's whole way to its give-up is about 2 hours of cooling.
    #[test]
    fn the_cooling_is_free_at_the_first_failed_run_then_doubles_up_to_fifteen_minutes() {
        let minutes = |n: u64| Some(Duration::from_secs(n * 60));
        for (runs, want) in [
            (0, None),
            (1, None),
            (2, minutes(2)),
            (3, minutes(4)),
            (4, minutes(8)),
            (5, minutes(15)),
            (6, minutes(15)),
            (12, minutes(15)),
            (33, minutes(15)),
            (34, minutes(15)),
            (u32::MAX, minutes(15)),
        ] {
            assert_eq!(cooling_after(runs), want, "after {runs} failed runs");
        }
        assert_eq!(JOB_GIVES_UP_AT, 12);
        let whole: u64 = (1..JOB_GIVES_UP_AT)
            .filter_map(cooling_after)
            .map(|d| d.as_secs())
            .sum();
        assert_eq!(whole, 119 * 60, "the cooling on a job's way to its give-up");
    }

    /// A row cools until its time, not at it; what thawed is gone from both looks; the next wake is the earliest of the
    /// rows still cooling.
    #[test]
    fn a_row_cools_until_its_time_and_the_next_wake_is_the_earliest() {
        let shared = Shared::default();
        assert_eq!(shared.next_thaw(0), None);
        assert_eq!(shared.cooling(0), [0_i64; 0]);
        shared.cool(7, 2_000);
        shared.cool(3, 1_000);
        assert_eq!(shared.cooling(999), [3, 7]);
        assert_eq!(shared.next_thaw(999), Some(1_000));
        assert_eq!(shared.cooling(1_000), [7], "thawed at its time");
        assert_eq!(shared.next_thaw(1_000), Some(2_000));
        shared.cool(7, 5_000);
        assert_eq!(shared.next_thaw(2_000), Some(5_000), "a later failed run");
        assert_eq!(shared.next_thaw(5_000), None);
        assert_eq!(shared.cooling(5_000), [0_i64; 0]);
    }

    /// R2's answer to a `Transmit` of a cooling row, literal: `Transient` with the time left, and nothing once it thawed or
    /// for another row. (That the arm asks it and that it writes no frame: `connection::send::contract_*`.)
    #[test]
    fn a_transmit_of_a_cooling_row_is_told_the_time_left() {
        let shared = Shared::default();
        shared.cool(7, 120_000);
        let answer = shared.cooling_answer(7, 45_000).expect("it cools");
        assert_eq!(
            answer.kind,
            ErrorKind::Transient {
                retry_after: Some(Duration::from_secs(75))
            }
        );
        assert!(shared.cooling_answer(8, 45_000).is_none(), "another row");
        assert!(shared.cooling_answer(7, 120_000).is_none(), "thawed");
    }

    /// A kick with nobody waiting is kept for the next look, once.
    #[tokio::test]
    async fn a_kick_with_nobody_waiting_is_kept_for_the_next_look() {
        let shared = Shared::default();
        shared.kick();
        shared.kick();
        shared.kicked().await;
    }
}
