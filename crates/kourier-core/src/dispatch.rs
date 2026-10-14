//! The per-account outbox scheduler (§4.8 steps 2-4). It lives in the account's supervisor:
//! one FIFO per conversation, heads run only while the account's connection is up (`Syncing` or
//! `Online`, [`ConnState::can_send`]: as Signal Desktop, the outbox does not wait for the
//! catch-up, link feedback L6), at most
//! [`MAX_IN_FLIGHT`] transmits at a time and one per conversation. Each attempt is one
//! `Command::Transmit`; its outcome is recorded by [`outbox::record_outcome`] and decides the
//! backoff (on the injected clock) or a pause:
//!
//! - `Challenge` (for the whole send, or for some members of a `Partial`) and a rejected login
//!   pause the whole account until the user's answer to the challenge was accepted
//!   (`UiCommand::SubmitChallenge`), the time the network gave with the challenge has passed
//!   (its Retry-After: the waiting rows are then tried once, as Signal Desktop's
//!   `scheduleRetry` does, and pause again if the challenge still stands), the backend reports
//!   `Online` again after a change, the user retries a message, or the actor restarts. The
//!   wait is part of each row's retry budget: a row whose budget ends while the account waits
//!   gives up like one whose last attempt failed ([`outbox::give_up_overdue`]). The wait
//!   counts up to the time the challenge ends by itself, also when the core looks only
//!   later (after a suspend or a restart): a budget that ended after that time is owed its
//!   late attempt. A
//!   challenge a backend met outside a send (`BackendNotice::ChallengeRequired`) shows the
//!   same account state and ends the same ways, but holds no send;
//! - `UntrustedIdentity` (for the whole send, or for some members of a `Partial`) pauses that
//!   conversation's queue until `ConfirmIdentity` for the user, a retry of the head, or a
//!   cancel of the head.
//!
//! A challenge is kept across a core restart ([`outbox::StoredChallenge`], written in the
//! transaction that recorded the refused attempt): the first actor of the next run takes the
//! wait up where it was, with its token, the time it ends by itself and the time it began,
//! and the account shows the challenge again as soon as its backend is online. Nothing is
//! sent into a network that still wants its challenge solved. A stored challenge older than a
//! day is dropped, as Signal Desktop does. Inside a run the wait ends as described above.
//!
//! An identity pause is in memory (it survives actor restarts, not a core restart): after a
//! core restart the head is tried once and pauses again if the reason still holds. It is
//! bounded like the challenge: a row of the paused conversation whose budget ends while the
//! queue waits for the user gives up ([`outbox::give_up_overdue`]), so nothing typed more
//! than a day ago goes out when the user at last confirms the number. A row that was past
//! its budget before either wait began keeps the one late attempt it is owed, after the
//! wait; when that attempt is answered with a challenge or a safety number again, it fails
//! instead of waiting on. A retry or a confirmation that arrives while no actor runs still
//! lifts the pause (the supervisor tells the backend about the confirmation at its next
//! start). If recording an outcome fails, the row is queued again after a backoff instead of
//! holding its conversation as `Transmitting`. Everything else that is durable (ids,
//! attempts, backoff, `only`) is in `core.db`. A pause banner names the user whose safety
//! number to confirm (`UiEvent::Notice`), a challenge shows
//! `ConnState::NeedsUserAction(SolveChallenge)`.
//!
//! Nothing is sent before the account is linked: while a login or relink runs, the actor may
//! report `Online` before the core has accepted its `AccountReady` (and a relink to another
//! identity is refused only then), so the rows of the account wait for [`Dispatcher::linked`].
//! After a logout they stay in `core.db` and go out when the same account is linked again.
//!
//! Wall-clock changes (§9.6): backoff timers are armed for a wall-clock time. After a resume
//! from suspend the armed sleep is stale ([`Dispatcher::rearm`]); when the clock was set back,
//! the stored waits are cleared ([`outbox::clear_waits`]) so no row waits for the old time.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kourier_backend_api::{BackendNotice, Command, Sleep, TransmitOutcome};
use kourier_db::Priority;
use kourier_model::{AccountId, ConnState, ConversationId, MsgIdScope, UserId};
use tokio::sync::{mpsc, oneshot};

use crate::error::Result;
use crate::ingest::Changes;
use crate::outbox::{self, Attempt, Pause, Skip, StoredChallenge, Wait};
use crate::runtime::Shared;
use crate::store::lane;

/// Most transmits in flight per account (the actor's command queue holds 64).
pub(crate) const MAX_IN_FLIGHT: usize = 4;

/// How long a command may wait for room in the actor's queue.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// A signal from the core task (UI commands) to an account's dispatcher.
#[derive(Debug)]
pub(crate) enum Signal {
    /// The outbox changed (enqueue, cancel): look for due heads.
    Kick,
    /// The user retried a message of `conv`: lift its pauses (and the account's).
    Retried {
        /// The conversation.
        conv: ConversationId,
    },
    /// The network accepted the user's answer to the challenge: lift the account's pause.
    ChallengeSolved,
}

/// One finished attempt, posted by the task that awaited the backend's reply.
#[derive(Debug)]
pub(crate) struct Done {
    generation: u64,
    outbox: i64,
    outcome: Option<TransmitOutcome>,
}

/// What woke the dispatcher.
#[derive(Debug)]
pub(crate) enum Wake {
    /// A backoff elapsed.
    Timer,
    /// An attempt finished.
    Done(Done),
    /// The time the network gave with a challenge has passed: the pause is lifted and the
    /// account shows the backend's real connection state again.
    ChallengeOver,
}

/// What one look at the outbox did ([`Dispatcher::kick`]).
#[derive(Debug, Default)]
pub(crate) struct Kicked {
    /// Rows to invalidate (floors moved to `Sending`, or to `Failed` out of budget).
    pub changes: Changes,
    /// Edits, deletes and reactions that ran out of budget while the account waited for a
    /// challenge (rolled back), reported as `ActionFailed`.
    pub failed: Vec<outbox::FailedChange>,
}

/// What the supervisor does after an outcome.
#[derive(Debug, Default)]
pub(crate) struct Effect {
    /// Rows to invalidate.
    pub changes: Changes,
    /// Show this connection state (a challenge the user must solve).
    pub conn: Option<ConnState>,
    /// Show this banner (whose safety number to confirm for a paused queue).
    pub notice: Option<BackendNotice>,
    /// An edit, delete or reaction the network refused for good (rolled back), reported as
    /// `ActionFailed`.
    pub failed: Option<outbox::FailedChange>,
}

/// The scheduler state of one account.
pub(crate) struct Dispatcher {
    account: AccountId,
    scope: MsgIdScope,
    /// Bumped at every actor start: replies of an earlier actor are ignored.
    generation: u64,
    /// The attempts in flight by outbox row. Row ids are never reused, so the answer to an
    /// attempt whose row went meanwhile (an echo, an expired or removed message) finds no
    /// newer row under its id.
    in_flight: HashMap<i64, Attempt>,
    done_tx: mpsc::UnboundedSender<Done>,
    done_rx: mpsc::UnboundedReceiver<Done>,
    /// The armed backoff timer: deadline (ms) and the clock's sleep. In a `Mutex` only so the
    /// supervisor stays `Sync` (it is polled through `&mut`, never locked).
    timer: Option<(i64, Mutex<Sleep>)>,
    /// The backend's last state was `Online` (caught up): the challenge rules go by it.
    online: bool,
    /// The connection is up (`Syncing` or `Online`): the outbox drains.
    up: bool,
    /// Since when (ms) the backend has not been `Online`, for the log's `after_ms`.
    offline_since: Option<i64>,
    /// The account's outbox waits (challenge, rejected credentials).
    account_paused: bool,
    /// What the UI shows while the account's outbox waits for a solved challenge.
    challenge: Option<ConnState>,
    /// The token of that challenge, handed back to the backend with the user's answer.
    challenge_token: Option<kourier_model::Redacted<String>>,
    /// A backend job met the challenge (`BackendNotice::ChallengeRequired`): it is shown like
    /// a challenged send, but the outbox is not paused for it.
    announced: bool,
    /// That announcement has been on show while the backend was `Online`: the backend
    /// coming `Online` anew then ends it (one that arrived during a catch-up is still due).
    announced_shown: bool,
    /// The account's outbox was paused by a challenge met while the connection was up but
    /// still catching up (`Syncing`; the outbox sends there too, link feedback L6). The
    /// `Online` that ends that catch-up is the same connection, not a return after an
    /// interruption, so it does not lift the pause; the next connection's `Online` does. The
    /// challenge is also on show over the catch-up's own updates.
    paused_in_catch_up: bool,
    /// When the challenge ends by itself (the network's Retry-After): the deadline (ms) and
    /// the clock's sleep, which is `None` after a resume from suspend until it is armed again.
    challenge_end: Option<(i64, Option<Mutex<Sleep>>)>,
    /// The connection state the backend published last.
    backend_conn: ConnState,
    /// Paused conversation heads: conversation → (outbox row, user to confirm, when the
    /// pause began in ms).
    paused: BTreeMap<ConversationId, (i64, UserId, i64)>,
    /// When the challenge on show began (ms): the budgets that end after it are used up by
    /// the wait.
    challenge_since: Option<i64>,
    /// The challenge as `core.db` has it ([`Self::sync_challenge`] writes what differs).
    stored: Option<StoredChallenge>,
    /// The pause was taken up from `core.db` at the start of this run and the backend has
    /// not been online since: its first `Online` is not "online anew", it is the state the
    /// pause is shown under.
    restored: bool,
    /// An actor of this account was started before in this run.
    started: bool,
    /// The core accepted this actor's identity (`AccountReady`): only then is anything sent.
    linked: bool,
    /// The clock at the last look at the outbox, to notice that it was set back.
    last_now: Option<i64>,
}

/// A clock that reads this much less than at the last look was set back (smaller steps are
/// ordinary corrections, which at worst delay a retry by as much).
const CLOCK_WENT_BACK: Duration = Duration::from_secs(10);

impl Dispatcher {
    pub(crate) fn new(account: AccountId, scope: MsgIdScope) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            account,
            scope,
            generation: 0,
            in_flight: HashMap::new(),
            done_tx,
            done_rx,
            timer: None,
            online: false,
            up: false,
            offline_since: None,
            account_paused: false,
            challenge: None,
            challenge_token: None,
            announced: false,
            announced_shown: false,
            paused_in_catch_up: false,
            challenge_end: None,
            backend_conn: ConnState::default(),
            paused: BTreeMap::new(),
            challenge_since: None,
            stored: None,
            restored: false,
            started: false,
            linked: false,
            last_now: None,
        }
    }

    /// Forgets the challenge (answered, timed out, or the actor it came from is gone); `true`
    /// when one was on show, so the account's real connection state must be shown again.
    fn clear_challenge(&mut self) -> bool {
        let shown = self.challenge.take().is_some();
        self.challenge_token = None;
        self.challenge_end = None;
        self.announced = false;
        self.announced_shown = false;
        self.paused_in_catch_up = false;
        self.challenge_since = None;
        self.restored = false;
        std::mem::replace(&mut self.account_paused, false) || shown
    }

    /// The challenge this account waits on, as it is to be kept across a restart: the one
    /// a send was refused with (with its token), or the one a job of the backend met.
    fn challenge_to_store(&self) -> Option<StoredChallenge> {
        let Some(ConnState::NeedsUserAction(kourier_model::UserAction::SolveChallenge { url })) =
            &self.challenge
        else {
            return None;
        };
        if !self.account_paused && !self.announced {
            return None;
        }
        Some(StoredChallenge {
            url: url.clone(),
            token: self.challenge_token.clone().filter(|_| self.account_paused),
            retry_at: self.challenge_end.as_ref().map(|(at, _)| *at),
            since: self.challenge_since?,
        })
    }

    /// Brings what `core.db` keeps of the challenge in line with the state here: forgotten
    /// when the wait ended, stored when a job announced one. (A challenged send is stored by
    /// the transaction that recorded it, [`outbox::record_outcome`]; an answer the network
    /// accepted forgets the challenge in its own write, before this dispatcher hears of it,
    /// `runtime::submit_challenge`.)
    pub(crate) async fn sync_challenge(&mut self, sh: &Arc<Shared>) {
        let want = self.challenge_to_store();
        if want == self.stored {
            return;
        }
        let (account, write) = (self.account, want.clone());
        let res = sh
            .store
            .write(Priority::High, lane(account), move |c| {
                outbox::store_challenge(c, account, write.as_ref())
            })
            .await;
        match res {
            Ok(()) => self.stored = want,
            // Tried again at the next look. A stored challenge that should be gone is
            // taken up at the next start and ends there by its time, its age or the user.
            Err(e) => {
                tracing::warn!(account = account.0, error = %e, "storing the challenge state failed")
            }
        }
    }

    /// Arms the end of the challenge `after` from now, in place of whatever was armed: the
    /// network counts from its newest answer. `None`: the challenge waits for the user.
    fn end_challenge_after(&mut self, sh: &Shared, after: Option<Duration>) {
        self.challenge_end = after.map(|d| {
            let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
            let at = sh.clock.now_ms().saturating_add(ms);
            (
                i64::try_from(at).unwrap_or(i64::MAX),
                Some(Mutex::new(sh.clock.sleep_until(at))),
            )
        });
    }

    /// `BackendNotice::ChallengeRequired`: a job of the backend met a challenge. The account
    /// shows it like a challenged send; nothing is paused. Returns the connection state to
    /// show now. A send that is already paused on a challenge keeps its own (its token is the
    /// one the answer needs).
    pub(crate) fn announce_challenge(
        &mut self,
        sh: &Shared,
        url: String,
        retry_after: Option<Duration>,
    ) -> ConnState {
        if !self.account_paused {
            self.challenge = Some(ConnState::NeedsUserAction(
                kourier_model::UserAction::SolveChallenge { url },
            ));
            self.announced = true;
            self.announced_shown = self.online;
            self.challenge_since = Some(sh.now());
            self.end_challenge_after(sh, retry_after);
        }
        self.shown(self.backend_conn.clone())
    }

    /// The connection state to show right now (the backend's, or the challenge over it).
    pub(crate) fn shown_now(&self) -> ConnState {
        self.shown(self.backend_conn.clone())
    }

    /// The core accepted the actor's `AccountReady`: its outbox may be sent now.
    pub(crate) fn linked(&mut self) {
        self.linked = true;
    }

    /// Whether the last connection state the backend published was `Online`.
    pub(crate) fn online(&self) -> bool {
        self.online
    }

    /// The armed sleeps are stale (the machine was suspended, and timers do not run then):
    /// the next [`kick`](Self::kick) arms them again for the time that is left.
    pub(crate) fn rearm(&mut self) {
        self.timer = None;
        if let Some((_, sleep)) = &mut self.challenge_end {
            *sleep = None;
        }
    }

    /// A new actor runs: rows an earlier one left `Transmitting` are queued again with their
    /// stored ids (§4.8 step 4); nothing is in flight. `linked`: the account is linked already
    /// (a start with stored credentials); a login reports it with [`linked`](Self::linked).
    ///
    /// The first actor of a run takes up the challenge the account waited on when the last
    /// run ended (module docs). A later actor of the same run, and a login, start without
    /// one: the stored challenge is forgotten.
    pub(crate) async fn start(&mut self, sh: &Arc<Shared>, linked: bool) {
        self.linked = linked;
        self.generation = self.generation.wrapping_add(1);
        self.in_flight.clear();
        self.timer = None;
        self.online = false;
        self.up = false;
        self.backend_conn = ConnState::default();
        self.clear_challenge();
        let account = self.account;
        let first = !std::mem::replace(&mut self.started, true);
        let now = sh.now();
        let reset = sh
            .store
            .write(Priority::High, lane(account), move |c| {
                let rows = outbox::reset_transmitting(c, account)?;
                let stored = outbox::stored_challenge(c, account)?;
                let kept = stored.filter(|s| first && linked && s.is_fresh(now));
                if kept.is_none() {
                    outbox::store_challenge(c, account, None)?;
                }
                Ok((rows, kept))
            })
            .await;
        match reset {
            Ok((rows, kept)) => {
                if rows > 0 {
                    tracing::info!(account = account.0, rows, "resuming interrupted sends");
                }
                self.stored = None;
                if let Some(stored) = kept {
                    self.take_up(sh, stored);
                }
            }
            Err(e) => {
                tracing::warn!(account = account.0, error = %e, "resetting the outbox failed")
            }
        }
    }

    /// Takes up a challenge that was stored when the last run ended.
    fn take_up(&mut self, sh: &Shared, stored: StoredChallenge) {
        tracing::info!(
            account = self.account.0,
            send = stored.token.is_some(),
            "the account still waits on a challenge from the last run"
        );
        self.challenge = Some(ConnState::NeedsUserAction(
            kourier_model::UserAction::SolveChallenge {
                url: stored.url.clone(),
            },
        ));
        self.challenge_since = Some(stored.since);
        self.challenge_end = stored.retry_at.map(|at| {
            let deadline = u64::try_from(at.max(0)).unwrap_or(0);
            (at, Some(Mutex::new(sh.clock.sleep_until(deadline))))
        });
        match &stored.token {
            Some(token) => {
                self.account_paused = true;
                self.challenge_token = Some(token.clone());
                self.restored = true;
            }
            // A job's challenge: on show from the backend's first `Online`.
            None => self.announced = true,
        }
        self.stored = Some(stored);
    }

    /// The backend published a connection state at `now` (ms); `true` when the outbox may
    /// have work now.
    pub(crate) fn conn(&mut self, state: &ConnState, now: i64) -> bool {
        let was = self.online;
        let was_up = self.up;
        self.online = state.is_online();
        self.up = state.can_send();
        self.backend_conn = state.clone();
        if !self.online && self.offline_since.is_none() {
            self.offline_since = Some(now);
        }
        if !self.up {
            // The connection the pause was met on is gone: the next `Online` is a new one.
            self.paused_in_catch_up = false;
        }
        if self.online && !was {
            // How long the account could not send everything: the connect and the catch-up
            // (diagnosis §2 item 4).
            let after_ms = self.offline_since.take().map(|t| now.saturating_sub(t));
            tracing::info!(
                account = self.account.0,
                after_ms,
                in_flight = self.in_flight.len(),
                "account online"
            );
            if std::mem::take(&mut self.restored) {
                // The pause was taken up from the last run: this is the first `Online` it
                // is shown under, not a return after an interruption.
            } else if std::mem::take(&mut self.paused_in_catch_up) {
                // The end of the catch-up the send met its challenge in: the same connection.
            } else if self.account_paused || self.announced_shown {
                // The backend is back after an interruption (for example a solved challenge).
                self.clear_challenge();
            } else if self.announced {
                // Announced during a catch-up: on show from now on.
                self.announced_shown = true;
            }
        }
        // A connection that came up drains what waits; `Online` after a catch-up may lift a
        // challenge's pause (above), which frees the outbox too.
        (self.up && !was_up) || (self.online && !was)
    }

    /// The token of the challenge the account's outbox waits on, if it waits on one.
    pub(crate) fn challenge_token(&self) -> Option<kourier_model::Redacted<String>> {
        self.challenge_token.clone().filter(|_| self.account_paused)
    }

    /// The connection state to show for the backend's `state`: while the outbox waits for a
    /// challenge (or a job of the backend met one) and the backend stays online, the challenge
    /// (a repeated `Online` must not hide it).
    pub(crate) fn shown(&self, state: ConnState) -> ConnState {
        match &self.challenge {
            Some(c)
                if (self.account_paused || self.announced)
                    && (state.is_online() || (self.paused_in_catch_up && state.can_send())) =>
            {
                c.clone()
            }
            _ => state,
        }
    }

    /// A signal from the core task; `true` when it lifted the account's pause or took a
    /// challenge off show (the supervisor then shows the backend's real connection state
    /// again).
    pub(crate) fn signal(&mut self, s: Signal) -> bool {
        match s {
            Signal::Kick => false,
            Signal::Retried { conv } => {
                self.paused.remove(&conv);
                // The user wants a send tried: a pause of the sends ends. A challenge that
                // only a job of the backend met holds no send and stays on show: nothing
                // the user did here answered it.
                if self.account_paused {
                    self.clear_challenge()
                } else {
                    false
                }
            }
            Signal::ChallengeSolved => self.clear_challenge(),
        }
    }

    /// `ConfirmIdentity` for `user`: the conversations paused on that user continue. Returns the
    /// heads that were paused (their messages show `Queued` again).
    pub(crate) fn confirmed(&mut self, user: &UserId) -> Vec<i64> {
        let (resumed, kept): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut self.paused)
            .into_iter()
            .partition(|(_, (_, u, _))| u == user);
        self.paused = kept;
        resumed.into_values().map(|(outbox, _, _)| outbox).collect()
    }

    /// Waits for a backoff to elapse or an attempt to finish (cancel-safe).
    pub(crate) async fn wake(&mut self) -> Wake {
        let Self {
            timer,
            done_rx,
            challenge_end,
            ..
        } = self;
        let sleep = async {
            match timer.as_mut() {
                Some((_, sleep)) => {
                    sleep
                        .get_mut()
                        .unwrap_or_else(PoisonError::into_inner)
                        .await
                }
                None => std::future::pending().await,
            }
        };
        let challenge = async {
            match challenge_end.as_mut() {
                Some((_, Some(sleep))) => {
                    sleep
                        .get_mut()
                        .unwrap_or_else(PoisonError::into_inner)
                        .await
                }
                _ => std::future::pending().await,
            }
        };
        let woke = tokio::select! {
            Some(done) = done_rx.recv() => Wake::Done(done),
            () = sleep => Wake::Timer,
            () = challenge => Wake::ChallengeOver,
        };
        match woke {
            Wake::Timer => self.timer = None,
            Wake::ChallengeOver => {
                tracing::info!(
                    account = self.account.0,
                    "the challenge's retry time has passed; trying the waiting sends"
                );
                self.clear_challenge();
            }
            Wake::Done(_) => {}
        }
        woke
    }

    /// Arms the timer for `at` (a backoff or a budget end), or disarms it.
    fn arm(&mut self, sh: &Shared, at: Option<i64>) {
        match at {
            Some(at) if self.timer.as_ref().is_none_or(|(t, _)| *t != at) => {
                let deadline = u64::try_from(at.max(0)).unwrap_or(0);
                self.timer = Some((at, Mutex::new(sh.clock.sleep_until(deadline))));
            }
            Some(_) => {}
            None => self.timer = None,
        }
    }

    /// The waits for the user that hold rows right now: the account's challenge, and every
    /// conversation whose queue waits for a safety number.
    fn waits(&self) -> Vec<Wait> {
        let mut waits = Vec::new();
        if self.account_paused
            && self.challenge.is_some()
            && let Some(since) = self.challenge_since
        {
            waits.push(Wait {
                conv: None,
                since,
                // A challenge that ends by itself holds rows until then, also when the
                // core looks later (the machine slept through it, or Kourier was closed).
                until: self.challenge_end.as_ref().map(|(at, _)| *at),
            });
        }
        waits.extend(self.paused.iter().map(|(conv, (_, _, since))| Wait {
            conv: Some(*conv),
            since: *since,
            until: None,
        }));
        waits
    }

    /// The rows whose retry budget ended while they waited for the user give up
    /// ([`outbox::give_up_overdue`]). Returns what that changed and the next budget end.
    async fn give_up_overdue(
        &mut self,
        sh: &Arc<Shared>,
        waits: Vec<Wait>,
    ) -> Result<(Kicked, Option<i64>)> {
        let (account, scope) = (self.account, self.scope);
        let now = i64::try_from(sh.clock.now_ms()).unwrap_or(i64::MAX);
        let gave = sh
            .store
            .write(Priority::High, lane(account), move |c| {
                outbox::give_up_overdue(c, account, scope, now, &waits)
            })
            .await?;
        if !gave.failed.is_empty() || !gave.changes.messages.is_empty() {
            tracing::info!(
                account = account.0,
                "sends gave up: their retry budget ended while they waited for the user"
            );
        }
        Ok((
            Kicked {
                changes: gave.changes,
                failed: gave.failed,
            },
            gave.next_deadline,
        ))
    }

    /// Starts every due head (one per conversation, at most [`MAX_IN_FLIGHT`] in flight) and arms
    /// the timer for the earliest backoff. Returns what changed (floors moved to `Sending`).
    /// While the account waits for a challenge nothing starts; the rows that ran out of budget
    /// give up instead, and so do those of a conversation that waits for a safety number.
    pub(crate) async fn kick(
        &mut self,
        sh: &Arc<Shared>,
        commands: &mpsc::Sender<kourier_backend_api::Command>,
    ) -> Result<Kicked> {
        self.sync_challenge(sh).await;
        // After a resume from suspend the end of a challenge is armed anew for what is left.
        if let Some((at, sleep @ None)) = &mut self.challenge_end {
            let deadline = u64::try_from((*at).max(0)).unwrap_or(0);
            *sleep = Some(Mutex::new(sh.clock.sleep_until(deadline)));
        }
        // Not for a rejected login (a pause without a challenge): what waits through a logout
        // goes out when the same account is linked again, however long that takes.
        if self.linked && self.account_paused && self.challenge.is_some() {
            let (kicked, next) = self.give_up_overdue(sh, self.waits()).await?;
            self.arm(sh, next);
            return Ok(kicked);
        }
        if !self.linked || !self.up || self.account_paused {
            return Ok(Kicked::default());
        }
        let now = i64::try_from(sh.clock.now_ms()).unwrap_or(i64::MAX);
        let back = i64::try_from(CLOCK_WENT_BACK.as_millis()).unwrap_or(i64::MAX);
        if self
            .last_now
            .is_some_and(|last| now < last.saturating_sub(back))
        {
            let account = self.account;
            let cleared = sh
                .store
                .write(Priority::High, lane(account), move |c| {
                    outbox::clear_waits(c, account)
                })
                .await?;
            tracing::info!(
                account = account.0,
                rows = cleared,
                "the clock went back; outbox waits cleared"
            );
            self.timer = None;
        }
        self.last_now = Some(now);
        // A conversation that waits for a safety number: what ran out of budget there gives
        // up first, so a head that did is no longer the head below.
        let (mut kicked, budget_end) = match self.waits() {
            waits if waits.is_empty() => (Kicked::default(), None),
            waits => self.give_up_overdue(sh, waits).await?,
        };
        let free = MAX_IN_FLIGHT.saturating_sub(self.in_flight.len());
        let skip = Skip {
            in_flight: self.in_flight.values().map(|a| a.conv).collect(),
            paused: self.paused.iter().map(|(c, (o, _, _))| (*c, *o)).collect(),
        };
        let (account, scope) = (self.account, self.scope);
        let due = sh
            .store
            .write(Priority::High, lane(account), move |c| {
                outbox::take_due(c, account, scope, now, &skip, free)
            })
            .await?;
        // A pause ends when its head is gone (cancelled, given up, or echoed from another
        // device).
        self.paused
            .retain(|conv, (outbox, _, _)| due.heads.get(conv) == Some(outbox));
        let wake = match (
            due.next_wake,
            budget_end.filter(|_| !self.paused.is_empty()),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.arm(sh, wake);
        for attempt in due.attempts {
            self.transmit(commands, attempt).await;
        }
        kicked.changes.absorb(due.changes);
        Ok(kicked)
    }

    /// Sends one attempt; a task awaits the reply and posts it back.
    async fn transmit(&mut self, commands: &mpsc::Sender<Command>, attempt: Attempt) {
        let (reply, rx) = oneshot::channel();
        let cmd = Command::Transmit {
            local: attempt.message,
            req: Box::new(attempt.req.clone()),
            reply,
        };
        let outbox = attempt.outbox;
        tracing::info!(
            account = self.account.0,
            conv = attempt.conv.0,
            msg = attempt.message.0,
            kind = attempt.req.kind.name(),
            attempt = attempt.number,
            waited_ms = attempt.started_at.saturating_sub(attempt.req.sent_at),
            in_flight = self.in_flight.len(),
            online = self.online,
            "transmit started"
        );
        self.in_flight.insert(outbox, attempt);
        match tokio::time::timeout(SEND_TIMEOUT, commands.send(cmd)).await {
            Ok(Ok(())) => {}
            // The reply sender went with the command: the waiter reads "no reply" below.
            Ok(Err(_)) => tracing::debug!(
                account = self.account.0,
                "account actor is gone; transmit dropped"
            ),
            Err(_) => tracing::warn!(
                account = self.account.0,
                "account actor does not read commands; transmit dropped"
            ),
        }
        let (tx, generation) = (self.done_tx.clone(), self.generation);
        tokio::spawn(async move {
            let outcome = rx.await.ok();
            let _ = tx.send(Done {
                generation,
                outbox,
                outcome,
            });
        });
    }

    /// Records a finished attempt (ignored when it belongs to an earlier actor).
    pub(crate) async fn done(&mut self, sh: &Arc<Shared>, done: Done) -> Result<Effect> {
        if done.generation != self.generation {
            return Ok(Effect::default());
        }
        let Some(attempt) = self.in_flight.remove(&done.outbox) else {
            return Ok(Effect::default());
        };
        let (account, scope) = (self.account, self.scope);
        let now = i64::try_from(sh.clock.now_ms()).unwrap_or(i64::MAX);
        let name = match &done.outcome {
            Some(TransmitOutcome::Sent { .. }) => "Sent",
            Some(TransmitOutcome::Partial { .. }) => "Partial",
            Some(TransmitOutcome::Failed(e)) => e.kind.name(),
            None => "no reply",
        };
        let (message, number, took_ms) = (
            attempt.message.0,
            attempt.number,
            now.saturating_sub(attempt.started_at),
        );
        let outcome = done.outcome;
        let recorded = match sh
            .store
            .write(Priority::High, lane(account), move |c| {
                outbox::record_outcome(c, account, scope, now, &attempt, outcome)
            })
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Not left `Transmitting` (that would hold the conversation's FIFO until the next
                // actor start): queued again after a backoff with the same id.
                let outbox_row = done.outbox;
                let released = sh
                    .store
                    .write(Priority::High, lane(account), move |c| {
                        outbox::release(c, outbox_row, now)
                    })
                    .await;
                if let Err(e2) = released {
                    tracing::warn!(account = account.0, error = %e2, "requeueing an unrecorded attempt failed");
                }
                return Err(e);
            }
        };
        // At the end, with the next try the outcome chose (diagnosis §2 item 2).
        tracing::info!(
            account = account.0,
            msg = message,
            attempt = number,
            outcome = name,
            took_ms,
            next_try_ms = recorded.retry_in.map(outbox::ms),
            paused = recorded.pause.is_some(),
            "transmit finished"
        );
        let mut effect = Effect {
            changes: recorded.changes,
            failed: recorded.failed,
            ..Effect::default()
        };
        match recorded.pause {
            Some(Pause::Conversation { conv, outbox, user }) => {
                tracing::info!(
                    account = account.0,
                    conv = conv.0,
                    "safety number changed; conversation queue paused"
                );
                effect.notice = Some(BackendNotice::IdentityChanged { user: user.clone() });
                self.paused.insert(conv, (outbox, user, now));
            }
            Some(Pause::Account { challenge }) => {
                tracing::info!(
                    account = account.0,
                    challenge = challenge.is_some(),
                    "account outbox paused"
                );
                // A challenged send replaces one that was only announced: its token is the
                // one the answer needs.
                self.clear_challenge();
                self.account_paused = true;
                self.paused_in_catch_up = self.up && !self.online;
                if let Some(c) = challenge {
                    // As the transaction that recorded the answer stored it.
                    self.stored = Some(c.stored(now));
                    self.challenge_since = Some(now);
                    self.challenge_end = c.retry_after.map(|d| {
                        let at = now.saturating_add(outbox::ms(d));
                        let deadline = u64::try_from(at.max(0)).unwrap_or(0);
                        (at, Some(Mutex::new(sh.clock.sleep_until(deadline))))
                    });
                    self.challenge_token = Some(c.token);
                    self.challenge = Some(ConnState::NeedsUserAction(
                        kourier_model::UserAction::SolveChallenge { url: c.url },
                    ));
                }
                effect.conn.clone_from(&self.challenge);
            }
            None => {}
        }
        Ok(effect)
    }
}
