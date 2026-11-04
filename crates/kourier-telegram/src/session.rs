//! The live session of one helper (TG-2 brief §3.2-§3.5; critic H1, M3, M4, M7; C8a): what the
//! account actor does with TDLib's updates, the answers of its requests and the person's input
//! while a helper runs, for a login and for a linked account alike.
//!
//! The session is STATE AND HANDLERS ONLY: no `select!`, no `drive`, no I/O. Every handler returns
//! the [`Act`]s the actor carries out in order (C8b's `Wake` loop: an inbound frame, a request's
//! answer, a command, a tick each become ONE handler call after the actor's `select!`):
//!
//! - **Requests** (H1): at most ONE request is in flight ([`Session::in_flight`]); a second one
//!   waits in the session and is handed out when the first one's answer came
//!   ([`Session::on_answer`]). The actor builds the in-flight future from a CLONE of the helper's
//!   link (`request_secret`), so a command that wins the actor's `select!` never drops an answer.
//! - **A login** ([`Session::login`]): every authorization state goes to the login machine
//!   ([`Login`]); its steps, requests, the helper restart and the `AccountReady` come out as acts.
//!   `done` follows the core's ack ([`Session::on_acked`]), then the session is LINKED and the
//!   helper stays up. A login that ended `Failed` makes the session quiet: the core's `Logout`
//!   that follows the failed step does the rest (C8b).
//! - **A linked account** ([`Session::resume`], or a login that reached `done`): connection states
//!   become `ConnState` only while TDLib says `Ready` (during a login the steps speak, §3.3); the
//!   hold is stated once per connect before the first `Syncing`/`Online` ([`Holds`], M4); TDLib's
//!   OWN logout after Ready and a resumed account TDLib no longer knows (any login state) end the
//!   session `LoggedOut{cause: Unknown}` + `Exit::LoggedOut`, never a `BackendEvent::Logout` (M7);
//!   a client that closes without a logout is a restart (TG-1b's back-off); a request's 401 is
//!   never a logout (no request of a linked session reads it as one); `my_id` naming another user
//!   than the account's stored one fails the session (TDLib's data is not this account's).
//! - **The start's frames** (M3): [`Session::on_first`] replays the updates the start saw before
//!   the inbound queue is read; when the start dropped some (over `FIRST_FRAMES`), the replay
//!   skips its authorization states and the session asks `getAuthorizationState` instead.
//!
//! - **The receive** (TG-3 C7a): from TDLib's `Ready` on, every receive update goes to the chat
//!   and message machines ([`Receiver`], one per helper run, [`Session::begin_run_seeded`]); the
//!   chat machine LEARNS before the link and stages only after it ([`Session::link`] links it). Their
//!   output becomes [`Act::Rows`] (one `store::commit_rows`), [`Act::Ephemeral`] and the sync
//!   lane's [`Act::Fetch`] ([`lane`]: one request in flight, the read act first, paced fetches, a
//!   429 pauses it). `SyncReadMarker` -> `viewMessages(force_read: true)` on that lane
//!   ([`Session::read_marker`], critic H3), its command confirmed after the last page together
//!   with the act's highest incoming id as the chat's stored read position (TG-4 K4); at the
//!   lists' load a read TDLib lost to a kill is sent again once per chat (the read re-assert,
//!   `LaneAsk::Reassert`, `Messages::lost_reads`).
//!
//! Nothing here logs a frame, a typed value, a name, a number or a link: fixed texts and fixed
//! classes only. `Debug` of an [`Act`] prints kinds, never a request's JSON.

use std::collections::VecDeque;
use std::fmt;

use kourier_backend_api::{BackendEvent, Exit};
use kourier_model::{AccountHold, BackendError, CmdId, ConnState, LoginStep, Redacted};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::error::{LinkError, TdClass};
use crate::login::{Login, Out, Ready, wipe};
use crate::receive::{self, Receiver};
use crate::states::{self, Holds};
use crate::store::{ChatFacts, Event, Row, Seed, Staged};
use crate::supervisor::FirstState;
use crate::tdapi::{self, AuthorizationState, ConnectionState, FreezeState, Update};

pub(crate) use crate::login::ReqKind;
pub(crate) use lane::LaneAsk;
use lane::{Done, Lane, Outcome};

pub(crate) mod lane;

/// The detail of a session whose TDLib data names another user than the account's.
pub(crate) const OTHER_ACCOUNT: &str = "Telegram's data of this account names another account";

/// The error that ends such a session (fix J3, Signal's rule for a wrong environment): UNSUPPORTED,
/// so the core halts the account with these words at once instead of restarting it, and every
/// restart would only start a gate and a helper over another user's TDLib data again.
pub(crate) fn other_account() -> BackendError {
    BackendError::unsupported(OTHER_ACCOUNT).with_user_message(
        "The Telegram data kept for this account belongs to another Telegram account. \
         Nothing was changed. Remove this account and add it again.",
    )
}

/// The error of a login whose `AccountReady` the core refused for good (or that could not be
/// written): the login ends at `linking`.
pub(crate) const NOT_TAKEN: &str = "the linked Telegram account could not be handed over";

/// What a request in flight answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Asked {
    /// A request of the login machine.
    Login(ReqKind),
    /// `getAuthorizationState` of the session itself (the start dropped frames, M3).
    State,
}

/// What the actor must do, in order.
pub(crate) enum Act {
    /// An ephemeral `BackendEvent::Login`.
    Step(LoginStep),
    /// Send `json` with `HelperLink::request_secret` on the helper's client; its answer goes back
    /// through [`Session::on_answer`] with `asked`. At most one is in flight.
    Send {
        asked: Asked,
        json: Zeroizing<String>,
    },
    /// Append this `AccountReady` with the user id (`outbox::append_ready`), ship it, then call
    /// [`Session::on_acked`] once the core acked its row, or [`Session::on_ready_failed`].
    /// (The event is boxed: it is the largest variant by far, clippy's `large_enum_variant`.)
    AppendReady {
        user_id: i64,
        event: Box<BackendEvent>,
    },
    /// The account's connection state (the actor's `watch`).
    Conn(ConnState),
    /// An ephemeral `BackendEvent::AccountHold`.
    Hold(Option<AccountHold>),
    /// Stop the helper and start it again after the back-off (TG-1b's restart), then call
    /// [`Session::on_restart`] and, with the new helper's first state, [`Session::on_first`].
    Restart,
    /// Stop the helper and end the actor with this exit.
    End(Exit),
    /// Commit these rows, facts and gaps in ONE transaction (`store::commit_rows`; boxed: the
    /// largest variant). C7b ships them with the shipping arm.
    Rows(Box<Staged>),
    /// An ephemeral event for the core at once (typing), never stored.
    Ephemeral(Box<BackendEvent>),
    /// Send `ask` on the sync lane (`Live.fetch`, never the login's slot); its answer goes back
    /// through [`Session::on_fetched`]. At most one is in flight.
    Fetch { ask: LaneAsk, json: Value },
    /// The start sync of this helper run is over (lead L-6): commit what is staged, then call
    /// [`Session::on_synced`] once the core acked every `core_outbox` row written up to now.
    Synced,
}

impl fmt::Debug for Act {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Kinds only: a request's JSON may hold a typed code or password, a step a QR link.
        match self {
            Self::Step(step) => write!(f, "Step({}, {})", step.step_id, step.txn),
            Self::Send { asked, .. } => write!(f, "Send({asked:?})"),
            Self::AppendReady { .. } => f.write_str("AppendReady"),
            Self::Conn(state) => write!(f, "Conn({state:?})"),
            Self::Hold(hold) => write!(f, "Hold({hold:?})"),
            Self::Restart => f.write_str("Restart"),
            Self::End(exit) => write!(f, "End({exit:?})"),
            Self::Rows(staged) => write!(f, "Rows({staged:?})"),
            Self::Ephemeral(event) => write!(f, "Ephemeral({})", event.name()),
            Self::Fetch { ask, .. } => write!(f, "Fetch({ask:?})"),
            Self::Synced => f.write_str("Synced"),
        }
    }
}

/// The start sync of one helper run (lead L-6): `Online` waits for its rows' acks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartSync {
    /// Not begun: TDLib's `Ready` shows `Syncing` and begins it.
    Running,
    /// The chat load and the fresh-link load run (C8): `Ready` shows `Syncing`.
    Loading,
    /// Over; [`Act::Synced`] was handed out and the acks are awaited.
    Acking,
    /// Its rows are acked: `Ready` is `Online` for the rest of the run.
    Done,
}

/// Where the session stands.
enum Phase {
    /// A login runs (boxed: the machine is the largest phase by far).
    Login(Box<Login>),
    /// The account is linked; the helper stays up.
    Linked,
    /// Nothing more comes out: the login ended `Failed`, or the session ended (`Act::End`).
    Quiet,
}

/// One account's live session (see the module doc).
pub(crate) struct Session {
    phase: Phase,
    /// The account's own TDLib user id: the stored one at a resume, the accepted one after a
    /// login's ack.
    user_id: Option<i64>,
    /// The user id of the `AccountReady` handed out, until the core acked it.
    awaiting_ack: Option<i64>,
    /// TDLib said `authorizationStateReady` on this helper (reset at a restart).
    authorized: bool,
    /// TDLib said `authorizationStateReady` on ANY helper of this session: a Telegram session
    /// exists, whatever a restart did since (fix J6; never reset).
    ever_authorized: bool,
    /// Kourier asked TDLib to log out (C8b's logout, L5): its LoggingOut and Closed are no end.
    asked_logout: bool,
    holds: Holds,
    /// A freeze TDLib stated before the link (stated at the link).
    freeze_seen: Option<FreezeState>,
    /// The last connection state TDLib said (also during a login; C8c's network hint reads it).
    last_conn: Option<ConnectionState>,
    /// The last `ConnState` this session handed out (no repeats).
    shown_conn: Option<ConnState>,
    in_flight: Option<Asked>,
    /// Requests that wait for the one in flight, oldest first.
    waiting: VecDeque<(Asked, Zeroizing<String>)>,
    /// The receive machines of this helper run ([`Session::begin_run_seeded`]; `None` before it).
    recv: Option<Receiver>,
    /// The sync lane (survives a restart: a read act goes on with the next helper).
    lane: Lane,
    /// This helper run's start sync (reset at a restart).
    sync: StartSync,
    /// The chat machine's bound before the link, when a test lowered it (TG-4 K6b); `None`: the
    /// product's (a test build only: U9).
    #[cfg(test)]
    learn_bound: Option<usize>,
}

impl Session {
    fn new(phase: Phase, user_id: Option<i64>) -> Self {
        Self {
            phase,
            user_id,
            awaiting_ack: None,
            authorized: false,
            ever_authorized: false,
            asked_logout: false,
            holds: Holds::default(),
            freeze_seen: None,
            last_conn: None,
            shown_conn: None,
            in_flight: None,
            waiting: VecDeque::new(),
            recv: None,
            lane: Lane::default(),
            sync: StartSync::Running,
            #[cfg(test)]
            learn_bound: None,
        }
    }

    /// A test's lowered bound for the chat machine of every helper run that begins from now on
    /// (`Timings::learn_bound`; `None`: the product's [`crate::receive::chats::LEARN_BOUND`]).
    #[cfg(test)]
    pub(crate) fn set_learn_bound(&mut self, bound: Option<usize>) {
        self.learn_bound = bound;
    }

    /// The session of a login (`StartLogin`).
    pub(crate) fn login(login: Login) -> Self {
        Self::new(Phase::Login(Box::new(login)), None)
    }

    /// The session of a linked account (`Connect` of a resumed account); `user_id` is the
    /// account row's.
    pub(crate) fn resume(user_id: Option<i64>) -> Self {
        Self::new(Phase::Linked, user_id)
    }

    /// The account is linked (a resume, or a login past `done`).
    pub(crate) fn is_linked(&self) -> bool {
        matches!(self.phase, Phase::Linked)
    }

    /// A login runs.
    pub(crate) fn in_login(&self) -> bool {
        matches!(self.phase, Phase::Login(_))
    }

    /// Nothing more comes out of this session.
    pub(crate) fn is_quiet(&self) -> bool {
        matches!(self.phase, Phase::Quiet)
    }

    /// TDLib said `Ready` on this helper (a session exists on Telegram's side: a logout must
    /// `logOut`, C8b).
    pub(crate) fn authorized(&self) -> bool {
        self.authorized
    }

    /// TDLib said `Ready` on some helper of this session, restarts included (J6): a logout must
    /// `logOut`, or report `NotReached` when no helper runs.
    pub(crate) fn ever_authorized(&self) -> bool {
        self.ever_authorized
    }

    /// The account's own user id, once known.
    pub(crate) fn user_id(&self) -> Option<i64> {
        self.user_id
    }

    /// The request in flight.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> Option<Asked> {
        self.in_flight
    }

    /// The last connection state TDLib said (C8c: a network hint only when it is not `Ready`).
    pub(crate) fn last_connection(&self) -> Option<ConnectionState> {
        self.last_conn
    }

    /// A new helper run begins (before [`Session::on_first`]): fresh receive machines (TDLib
    /// tells every chat again per instance: the message DB is off). `after_unclean`: the previous
    /// run did not end cleanly (`store::start_run`). A linked session links the chat machine at
    /// once (a resume knows its user id).
    #[cfg(test)]
    pub(crate) fn begin_run(&mut self, start_ms: u64, after_unclean: bool) -> Vec<Act> {
        self.begin_run_seeded(start_ms, after_unclean, &Seed::default())
    }

    /// A new helper run begins (before [`Session::on_first`]), the machines seeded from the store
    /// first (C8: the cursors, read positions and delivered messages of every chat,
    /// `store::seed`). Fresh receive machines: TDLib tells every chat again per instance (the
    /// message DB is off). `after_unclean`: the previous run did not end cleanly
    /// (`store::start_run`). A linked session links the chat machine at once.
    pub(crate) fn begin_run_seeded(
        &mut self,
        start_ms: u64,
        after_unclean: bool,
        seed: &Seed,
    ) -> Vec<Act> {
        let start = ms(start_ms);
        let mut recv = Receiver::new(self.user_id, start, after_unclean);
        #[cfg(test)]
        if let Some(bound) = self.learn_bound {
            recv.chats.set_learn_bound(bound);
        }
        recv.seed(seed);
        let out = match self.user_id {
            Some(me) if self.is_linked() => recv.chats.link(me),
            _ => receive::Out::default(),
        };
        self.recv = Some(recv);
        self.receive_outs(out, start_ms)
    }

    /// The patches the chat machine holds until a flush (run B Q-B2: once per commit batch, C7b).
    pub(crate) fn flush_receive(&mut self) -> Vec<Act> {
        let Some(recv) = self.recv.as_mut() else {
            return Vec::new();
        };
        let out = recv.chats.flush();
        self.receive_outs(out, 0)
    }

    /// Messages of a chat still wait for its `getChat`, or dropped ids have no gap row yet (TF3):
    /// a stop now is not a clean end.
    pub(crate) fn receive_unsettled(&self) -> bool {
        self.recv.as_ref().is_some_and(Receiver::unsettled)
    }

    /// Forgets the receive machines (TF1): a receive commit failed, so their memory is ahead of
    /// the file. The next helper run seeds them again from the store; until then they take and
    /// flush nothing.
    pub(crate) fn drop_receive(&mut self) {
        self.recv = None;
    }

    /// The person read `chat_id` here up to a marker: the delivered messages up to it (`rows`:
    /// `(id, outgoing)`, any order, the actor reads them from `delivered`) are told with
    /// `viewMessages(force_read: true)` on the sync lane AHEAD of the fetches (Q-L3, Q-L4), and
    /// `cmd` is confirmed after the last page. ONLY THE INCOMING IDS ARE TOLD (U2, honesty
    /// HIGH-1): TDLib takes the maximum of the ids it is told with no test of their direction
    /// (`MessagesManager.cpp:16715-16717`), so an outgoing id in the request would become its read
    /// position, its echo and the server request's `max_id`, which is not what the peer's side
    /// reports; the highest incoming id is also the position the confirm stores. With no incoming
    /// id (or in a held chat) there is nothing to tell and `cmd` is confirmed at once.
    pub(crate) fn read_marker(
        &mut self,
        chat_id: i64,
        rows: Vec<(i64, bool)>,
        cmd: CmdId,
        now_ms: u64,
    ) -> Vec<Act> {
        let mut acts = Vec::new();
        // A chat held now (TF11, review pF2: it became public, or a channel, after its rows came)
        // gets no read act: there is nothing to tell, and the command is confirmed.
        let held = self.recv.as_ref().is_some_and(|r| r.chats.is_held(chat_id));
        let ids: Vec<i64> = if held {
            Vec::new()
        } else {
            rows.into_iter()
                .filter(|(_, outgoing)| !outgoing)
                .map(|(id, _)| id)
                .collect()
        };
        let up_to = ids.iter().copied().max();
        if let Some(cmd) = self.lane.push_read(chat_id, ids, cmd, up_to) {
            acts.push(persisted(&[cmd]));
        }
        acts.extend(self.pump_lane(now_ms));
        acts
    }

    /// When the actor must call [`Session::pump_lane`] (the pace or a 429's pause), in the
    /// clock's ms.
    pub(crate) fn lane_due_ms(&self) -> Option<u64> {
        if self.is_quiet() {
            return None;
        }
        self.lane.due_ms()
    }

    /// The next request of the sync lane, if one may go now.
    pub(crate) fn pump_lane(&mut self, now_ms: u64) -> Option<Act> {
        if self.is_quiet() {
            return None;
        }
        let ask = self.lane.next(now_ms)?;
        let json = ask.request();
        Some(Act::Fetch { ask, json })
    }

    /// The sync lane's request in flight.
    #[cfg(test)]
    pub(crate) fn lane_in_flight(&self) -> Option<&LaneAsk> {
        self.lane.in_flight()
    }

    /// The answer of the sync lane's `ask` (as handed out in [`Act::Fetch`]).
    pub(crate) fn on_fetched(
        &mut self,
        ask: LaneAsk,
        result: Result<Value, LinkError>,
        now_ms: u64,
    ) -> Vec<Act> {
        let outcome = match &result {
            Ok(_) => Outcome::Ok,
            Err(LinkError::Auth {
                class: TdClass::TooManyRequests,
                retry_after_s,
                ..
            }) => Outcome::TooMany(*retry_after_s),
            Err(LinkError::Td { .. } | LinkError::Auth { .. } | LinkError::Refused(_)) => {
                Outcome::Refused
            }
            Err(_) => Outcome::Lost,
        };
        if matches!(outcome, Outcome::TooMany(_)) {
            tracing::debug!("a sync request was answered 429; the sync lane waits");
        }
        let mut acts = Vec::new();
        match self.lane.on_answer(ask, outcome, now_ms) {
            Done::Nothing => {}
            Done::Read {
                cmds,
                chat_id,
                up_to,
            } => acts.push(self.read_confirmed(&cmds, chat_id, up_to)),
            Done::Fetch(fetch) => {
                if let Some(recv) = self.recv.as_mut() {
                    let out = recv.on_fetched(fetch, result, ms(now_ms));
                    let loaded = recv.sync_done();
                    acts.extend(self.receive_outs(out, now_ms));
                    if loaded && self.sync == StartSync::Loading {
                        acts.extend(self.end_start_sync());
                    }
                }
            }
        }
        acts.extend(self.pump_lane(now_ms));
        acts
    }

    /// A read act's last page was answered `ok` (TG-4 K4, critic M3): the commands are confirmed
    /// and the act's highest incoming id becomes the chat's stored read position in the SAME
    /// commit (`CommandPersisted` rows and the fact are one `Staged`), so a kill never leaves the
    /// confirm without the position the re-assert compares with. TDLib's own echo
    /// (`updateChatReadInbox`) normally comes first and has staged the position already: then
    /// there is no second fact. The belt is for the postponed echo (TDLib holds its update while
    /// a difference runs, `MessagesManager.cpp:27302`): there the `ok` comes first.
    fn read_confirmed(&mut self, cmds: &[CmdId], chat_id: i64, up_to: Option<i64>) -> Act {
        let fact = up_to.and_then(|up_to| {
            self.recv
                .as_mut()
                .and_then(|recv| recv.messages.confirm_read(chat_id, up_to))
        });
        persisted_with(cmds, fact)
    }

    /// The login's `txn` (0: no login or no step yet).
    #[cfg(test)]
    pub(crate) fn txn(&self) -> u64 {
        match &self.phase {
            Phase::Login(login) => login.txn(),
            Phase::Linked | Phase::Quiet => 0,
        }
    }

    /// When [`Session::on_tick`] must be called next (the QR bound; a linked account's
    /// supergroup decision waiting for its stream, C9b), in the clock's milliseconds.
    pub(crate) fn deadline_ms(&self) -> Option<u64> {
        match &self.phase {
            Phase::Login(login) => login.deadline_ms(),
            Phase::Linked => self
                .recv
                .as_ref()
                .and_then(Receiver::due_ms)
                .map(|due| u64::try_from(due).unwrap_or(0)),
            Phase::Quiet => None,
        }
    }

    /// Kourier asks TDLib to log out now (C8b sends `logOut` right after): TDLib's LoggingOut,
    /// Closing and Closed are then no failure and no end of their own (L5).
    pub(crate) fn logout_asked(&mut self) {
        self.asked_logout = true;
        if let Phase::Login(login) = &mut self.phase {
            login.logout_asked();
        }
    }

    /// The helper's first state and the updates its start saw (M3), before the inbound queue.
    pub(crate) fn on_first(&mut self, first: FirstState, now_ms: u64) -> Vec<Act> {
        let FirstState {
            frames, dropped, ..
        } = first;
        let mut acts = Vec::new();
        if self.is_linked() {
            acts.extend(self.show(ConnState::Connecting));
        }
        let reread = dropped > 0;
        let restarting = |acts: &[Act]| acts.iter().any(|a| matches!(a, Act::Restart));
        for json in &frames {
            // A restart asked for (a client that closed: `Closing` then `Closed`) ends the
            // replay: the rest belongs to a helper that goes away, and a second `Restart` would
            // count a second failure of the back-off (fix J5).
            if self.is_quiet() || restarting(&acts) {
                break;
            }
            match tdapi::parse_update(json.as_str()) {
                // The replay is incomplete: its states may be stale or missing.
                Some(Update::AuthorizationState(_)) if reread => {}
                Some(update) => acts.extend(self.on_update(update, now_ms)),
                None => {}
            }
        }
        drop(frames);
        if reread && !self.is_quiet() && !restarting(&acts) {
            tracing::debug!(
                dropped,
                "the TDLib start dropped updates; its state is read again"
            );
            acts.extend(self.send(Asked::State, state_request()));
        }
        acts
    }

    /// One update of the inbound queue (`Inbound::Update`).
    pub(crate) fn on_inbound(&mut self, json: &str, now_ms: u64) -> Vec<Act> {
        match tdapi::parse_update(json) {
            Some(update) => self.on_update(update, now_ms),
            None => {
                tracing::debug!("a TDLib update this build cannot read was skipped");
                Vec::new()
            }
        }
    }

    /// The answer of the request in flight (`asked` as handed out in [`Act::Send`]).
    pub(crate) fn on_answer(
        &mut self,
        asked: Asked,
        result: Result<Value, LinkError>,
        now_ms: u64,
    ) -> Vec<Act> {
        if self.in_flight == Some(asked) {
            self.in_flight = None;
        }
        let mut acts = match asked {
            Asked::Login(kind) => {
                let outs = match &mut self.phase {
                    Phase::Login(login) => login.on_answer(kind, result, now_ms),
                    Phase::Linked | Phase::Quiet => Vec::new(),
                };
                self.login_outs(outs)
            }
            Asked::State => match result {
                Ok(v) => self.on_auth_state(AuthorizationState::from_value(v), now_ms),
                Err(LinkError::Timeout | LinkError::Closed) => vec![Act::Restart],
                Err(e) => {
                    tracing::debug!(error = %e, "reading TDLib's state failed");
                    Vec::new()
                }
            },
        };
        acts.extend(self.pump());
        acts
    }

    /// The person's answer to the login step `txn` (`SubmitLoginInput`). Without a login the
    /// values are dropped unread.
    pub(crate) fn on_input(
        &mut self,
        txn: u64,
        values: Vec<(String, Redacted<String>)>,
        now_ms: u64,
    ) -> Vec<Act> {
        let outs = match &mut self.phase {
            Phase::Login(login) => login.on_input(txn, values, now_ms),
            Phase::Linked | Phase::Quiet => {
                wipe(values);
                return Vec::new();
            }
        };
        self.login_outs(outs)
    }

    /// The clock reached [`Session::deadline_ms`].
    pub(crate) fn on_tick(&mut self, now_ms: u64) -> Vec<Act> {
        let outs = match &mut self.phase {
            Phase::Login(login) => login.on_tick(now_ms),
            Phase::Linked => {
                let Some(recv) = self.recv.as_mut() else {
                    return Vec::new();
                };
                let out = recv.on_tick(ms(now_ms));
                return self.receive_outs(out, now_ms);
            }
            Phase::Quiet => return Vec::new(),
        };
        self.login_outs(outs)
    }

    /// The actor's own terminal failure of a running login (the restart budget used up, a helper
    /// that cannot start again): the `Failed` step at the current step.
    pub(crate) fn fail_login(&mut self, error: BackendError) -> Vec<Act> {
        let outs = match &mut self.phase {
            Phase::Login(login) => login.fail(error),
            Phase::Linked | Phase::Quiet => return Vec::new(),
        };
        self.login_outs(outs)
    }

    /// The core acked the `AccountReady` of [`Act::AppendReady`]: `done`, then the account is
    /// linked and its connection is shown.
    pub(crate) fn on_acked(&mut self) -> Vec<Act> {
        let Phase::Login(login) = &mut self.phase else {
            return Vec::new();
        };
        let outs = login.acked();
        if outs.is_empty() {
            return Vec::new();
        }
        let mut acts: Vec<Act> = outs
            .into_iter()
            .filter_map(|out| match out {
                Out::Step(step) => Some(Act::Step(step)),
                Out::Request(_) | Out::Abort | Out::Refused(_) | Out::Ready(_) => None,
            })
            .collect();
        self.user_id = self.awaiting_ack.take();
        acts.extend(self.link());
        acts
    }

    /// The `AccountReady` could not be written, or the core refused it for good (quarantined):
    /// the login ends `Failed{Internal}` at `linking` (Q-8).
    pub(crate) fn on_ready_failed(&mut self) -> Vec<Act> {
        self.awaiting_ack = None;
        let outs = match &mut self.phase {
            Phase::Login(login) => {
                // The machine still waits for the ack: its `fail` reports at `linking`.
                login.fail(BackendError::internal(NOT_TAKEN))
            }
            Phase::Linked | Phase::Quiet => return Vec::new(),
        };
        self.login_outs(outs)
    }

    /// The helper was restarted (after [`Act::Restart`], or an inbound end the actor saw): what
    /// was in flight is gone, TDLib must say `Ready` again, the next connect states its hold
    /// again. [`Session::on_first`] follows with the new helper's first state.
    pub(crate) fn on_restart(&mut self) {
        self.in_flight = None;
        self.waiting.clear();
        self.recv = None;
        self.lane.on_restart();
        self.sync = StartSync::Running;
        self.authorized = false;
        self.shown_conn = None;
        self.last_conn = None;
        self.holds.on_connect();
        if let Phase::Login(login) = &mut self.phase {
            login.on_restart();
        }
    }

    fn on_update(&mut self, update: Update, now_ms: u64) -> Vec<Act> {
        if self.is_quiet() {
            return Vec::new();
        }
        match update {
            Update::AuthorizationState(state) => self.on_auth_state(state, now_ms),
            Update::ConnectionState(state) => {
                let was = self.last_conn.replace(state);
                if let Some(recv) = self.recv.as_mut() {
                    recv.messages
                        .set_updating(matches!(state, ConnectionState::Updating));
                }
                if !(self.is_linked() && self.authorized) {
                    return Vec::new();
                }
                let mut acts = self.show_connection(state);
                // C9b: `Updating -> Ready`, when TDLib shows it, takes the reconcile's decision
                // again in memory (G-8: nothing waits for it to come).
                if matches!(was, Some(ConnectionState::Updating))
                    && matches!(state, ConnectionState::Ready)
                    && let Some(recv) = self.recv.as_mut()
                {
                    let out = recv.redecide(ms(now_ms));
                    acts.extend(self.receive_outs(out, now_ms));
                }
                acts
            }
            Update::FreezeState(freeze) => {
                if self.is_linked() {
                    self.holds
                        .on_freeze(&freeze)
                        .map(Act::Hold)
                        .into_iter()
                        .collect()
                } else {
                    // Stated at the link (an `AccountHold` before `AccountReady` names no
                    // account yet).
                    self.freeze_seen = Some(freeze);
                    Vec::new()
                }
            }
            option @ Update::Option { .. } => match option.my_id() {
                Some(id) if self.is_linked() && self.user_id.is_some_and(|own| own != id) => {
                    tracing::debug!("TDLib's data names another user than the account's");
                    self.end(Exit::Failed(other_account()))
                }
                _ => Vec::new(),
            },
            // From TDLib's Ready on (H1: TDLib tells each chat once per instance, before the link
            // too); the machines stage only after the link.
            Update::Receive(update) => match self.recv.as_mut() {
                Some(recv) if self.authorized => {
                    let out = recv.on_update(&update, ms(now_ms));
                    self.receive_outs(out, now_ms)
                }
                _ => Vec::new(),
            },
            Update::Other => Vec::new(),
        }
    }

    fn on_auth_state(&mut self, state: AuthorizationState, now_ms: u64) -> Vec<Act> {
        if matches!(state, AuthorizationState::Ready) {
            self.authorized = true;
            self.ever_authorized = true;
        }
        if self.is_linked() {
            return self.linked_state(state);
        }
        let outs = match &mut self.phase {
            Phase::Login(login) => login.on_state(state, now_ms),
            Phase::Linked | Phase::Quiet => return Vec::new(),
        };
        self.login_outs(outs)
    }

    /// An authorization state of a linked account (§3.2).
    fn linked_state(&mut self, state: AuthorizationState) -> Vec<Act> {
        match state {
            AuthorizationState::Ready => match self.last_conn {
                Some(conn) => self.show_connection(conn),
                None => Vec::new(),
            },
            AuthorizationState::WaitTdlibParameters | AuthorizationState::Unknown => Vec::new(),
            AuthorizationState::LoggingOut
            | AuthorizationState::Closing
            | AuthorizationState::Closed
                if self.asked_logout =>
            {
                Vec::new()
            }
            // TDLib's OWN logout (the session was ended on another device, the account was
            // deleted or banned): M7, no `Logout` event.
            AuthorizationState::LoggingOut => self.logged_out(states::TELEGRAM_ENDED),
            // The client ends without a logout: the helper is going away (TG-1b's restart).
            AuthorizationState::Closing | AuthorizationState::Closed => vec![Act::Restart],
            // A linked account never sits in a login state: TDLib's session is gone (`tdlib/`
            // lost or reset, a login that never finished): L-8.
            AuthorizationState::WaitPhoneNumber
            | AuthorizationState::WaitOtherDeviceConfirmation { .. }
            | AuthorizationState::WaitCode { .. }
            | AuthorizationState::WaitPassword { .. }
            | AuthorizationState::WaitEmailAddress { .. }
            | AuthorizationState::WaitEmailCode { .. }
            | AuthorizationState::WaitRegistration
            | AuthorizationState::WaitPremiumPurchase => self.logged_out(states::SESSION_GONE),
        }
    }

    /// What the login machine handed out, as acts. A login that ended without `done` makes the
    /// session quiet.
    fn login_outs(&mut self, outs: Vec<Out>) -> Vec<Act> {
        let mut acts = Vec::new();
        for out in outs {
            match out {
                Out::Step(step) => acts.push(Act::Step(step)),
                Out::Request(req) => acts.extend(self.send(Asked::Login(req.kind), req.json)),
                Out::Abort => acts.push(Act::Restart),
                Out::Refused(refusal) => {
                    // The `Failed` step came just before; the core's `Logout` follows it (C8b
                    // logs the Telegram session out then).
                    tracing::debug!(?refusal, "a Telegram identity was refused after its login");
                }
                Out::Ready(ready) => match user_id_of(&ready) {
                    Some(user_id) => {
                        self.awaiting_ack = Some(user_id);
                        acts.push(Act::AppendReady {
                            user_id,
                            event: Box::new(ready.event()),
                        });
                    }
                    None => {
                        let outs = match &mut self.phase {
                            Phase::Login(login) => login.fail(BackendError::internal(NOT_TAKEN)),
                            Phase::Linked | Phase::Quiet => Vec::new(),
                        };
                        acts.extend(outs.into_iter().filter_map(|o| match o {
                            Out::Step(step) => Some(Act::Step(step)),
                            Out::Request(_) | Out::Abort | Out::Refused(_) | Out::Ready(_) => None,
                        }));
                    }
                },
            }
        }
        if let Phase::Login(login) = &self.phase
            && login.is_ended()
        {
            // Ended without `done` (`done` goes through `on_acked`, which links instead and never
            // comes here).
            self.phase = Phase::Quiet;
            self.waiting.clear();
        }
        acts
    }

    /// The login reached `done`: the account is linked; the freeze seen during the login and the
    /// connection are stated.
    fn link(&mut self) -> Vec<Act> {
        self.phase = Phase::Linked;
        self.holds.on_connect();
        let mut acts = Vec::new();
        if let (Some(me), Some(recv)) = (self.user_id, self.recv.as_mut()) {
            let out = recv.chats.link(me);
            acts.extend(self.receive_outs(out, 0));
        }
        if let Some(freeze) = self.freeze_seen.take()
            && let Some(hold) = self.holds.on_freeze(&freeze)
        {
            acts.push(Act::Hold(hold));
        }
        let shown = match self.last_conn {
            Some(conn) => self.show_connection(conn),
            None => Vec::new(),
        };
        if shown.is_empty() && self.shown_conn.is_none() {
            acts.extend(self.show(ConnState::Connecting));
        } else {
            acts.extend(shown);
        }
        acts
    }

    /// A TDLib connection state of a linked, authorized account (§3.3). `Ready` is `Online` only
    /// once this run's start sync is acked (lead L-6); before it `Ready` shows `Syncing`. No
    /// state at all (a short drop TDLib folds, G-8) changes nothing: Online stays.
    fn show_connection(&mut self, conn: ConnectionState) -> Vec<Act> {
        let mut acts = Vec::new();
        let state = match states::conn_of(conn) {
            Some(ConnState::Online { .. }) if self.sync != StartSync::Done => {
                if self.sync == StartSync::Running {
                    acts.extend(self.begin_start_sync());
                }
                ConnState::Syncing { fraction: None }
            }
            Some(state) => state,
            None => return acts,
        };
        let mut shown = self.show(state);
        shown.append(&mut acts);
        shown
    }

    /// The run's first `Ready` of a linked account begins the start sync (C8): the chat load on
    /// the sync lane (the actor's `Wake::LaneDue` sends it). Without receive machines (a session
    /// test that began no run) there is nothing to load and it ends at once.
    fn begin_start_sync(&mut self) -> Vec<Act> {
        let Some(recv) = self.recv.as_mut() else {
            return self.end_start_sync();
        };
        self.sync = StartSync::Loading;
        let out = recv.begin_sync();
        self.receive_outs(out, 0)
    }

    /// The start sync of this run is over (C8: both lists loaded and every fresh-link page
    /// answered): the message machine stops reading the backlog as catch-up, and the actor
    /// commits and waits for the acks ([`Act::Synced`]).
    fn end_start_sync(&mut self) -> Vec<Act> {
        self.sync = StartSync::Acking;
        if let Some(recv) = self.recv.as_mut() {
            recv.messages.start_sync_done();
        }
        vec![Act::Synced]
    }

    /// The core acked every row written up to [`Act::Synced`]: `Online` from now on in this
    /// run, at once when TDLib says `Ready`.
    pub(crate) fn on_synced(&mut self) -> Vec<Act> {
        if self.sync != StartSync::Acking || self.is_quiet() {
            return Vec::new();
        }
        self.sync = StartSync::Done;
        match self.last_conn {
            Some(ConnectionState::Ready) if self.is_linked() && self.authorized => {
                self.show_connection(ConnectionState::Ready)
            }
            _ => Vec::new(),
        }
    }

    /// `state`, with the connect's hold stated before the first `Syncing`/`Online` (M4); a
    /// repeat of the last state is not shown again.
    fn show(&mut self, state: ConnState) -> Vec<Act> {
        let mut acts = Vec::new();
        if let Some(hold) = self.holds.before(&state) {
            acts.push(Act::Hold(hold));
        }
        if self.shown_conn.as_ref() != Some(&state) {
            self.shown_conn = Some(state.clone());
            acts.push(Act::Conn(state));
        }
        acts
    }

    /// The end of a linked session by a logout TDLib did or found (M7).
    fn logged_out(&mut self, reason: &'static str) -> Vec<Act> {
        let mut acts = vec![Act::Conn(states::logged_out(reason))];
        acts.extend(self.end(Exit::LoggedOut {
            reason: reason.to_owned(),
        }));
        acts
    }

    fn end(&mut self, exit: Exit) -> Vec<Act> {
        self.phase = Phase::Quiet;
        self.waiting.clear();
        vec![Act::End(exit)]
    }

    /// `json` now, or after the request in flight.
    fn send(&mut self, asked: Asked, json: Zeroizing<String>) -> Option<Act> {
        if self.in_flight.is_some() {
            self.waiting.push_back((asked, json));
            None
        } else {
            self.in_flight = Some(asked);
            Some(Act::Send { asked, json })
        }
    }

    /// A machine's output as acts: the rows to commit, the ephemeral events, the fetches on the
    /// sync lane (the next one handed out when it may go).
    fn receive_outs(&mut self, out: receive::Out, now_ms: u64) -> Vec<Act> {
        let receive::Out {
            staged,
            fetch,
            reassert,
            ephemeral,
        } = out;
        let mut acts = Vec::new();
        if !staged.is_empty() {
            acts.push(Act::Rows(Box::new(staged)));
        }
        acts.extend(
            ephemeral
                .into_iter()
                .map(|event| Act::Ephemeral(Box::new(event))),
        );
        for f in fetch {
            self.lane.push_fetch(f);
        }
        if !reassert.is_empty() {
            tracing::debug!(count = reassert.len(), "reads TDLib lost are sent again");
        }
        for (chat_id, id) in reassert {
            self.lane.push_reassert(chat_id, id);
        }
        // 0: a caller without the clock (the link, a flush): the actor's `Wake::LaneDue` pumps.
        if now_ms > 0 {
            acts.extend(self.pump_lane(now_ms));
        }
        acts
    }

    /// The next waiting request, once nothing is in flight.
    fn pump(&mut self) -> Option<Act> {
        if self.in_flight.is_some() || self.is_quiet() {
            return None;
        }
        let (asked, json) = self.waiting.pop_front()?;
        self.in_flight = Some(asked);
        Some(Act::Send { asked, json })
    }
}

/// The rows that confirm durable commands (`CommandPersisted`, one row each).
pub(crate) fn persisted(cmds: &[CmdId]) -> Act {
    persisted_with(cmds, None)
}

/// [`persisted`] plus a chat fact in the same transaction.
fn persisted_with(cmds: &[CmdId], fact: Option<ChatFacts>) -> Act {
    let rows = cmds
        .iter()
        .map(|&cmd| Row {
            events: vec![Event {
                event: BackendEvent::CommandPersisted { cmd },
                gate: None,
            }],
            delivered: Vec::new(),
        })
        .collect();
    Act::Rows(Box::new(Staged {
        rows,
        chats: fact.into_iter().collect(),
        ..Staged::default()
    }))
}

/// A clock ms as the machines count it.
fn ms(now_ms: u64) -> i64 {
    i64::try_from(now_ms).unwrap_or(i64::MAX)
}

/// `getAuthorizationState` (no secret in it; the same zeroizing path as every request).
fn state_request() -> Zeroizing<String> {
    Zeroizing::new(String::from(r#"{"@type":"getAuthorizationState"}"#))
}

/// The `tg:` user id of an accepted identity (its primary id).
fn user_id_of(ready: &Ready) -> Option<i64> {
    let first = ready.own_ids.first()?;
    if first.scheme() != "tg" {
        return None;
    }
    first.value().parse::<i64>().ok().filter(|&id| id > 0)
}

#[cfg(test)]
mod tests;
