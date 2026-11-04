//! THE ATTEMPT (TH6a S6): what a `Command::Transmit` becomes behind the parts' test door, and every answer to the core.
//!
//! A `Transmit` is a question about a `local`, so the LEDGER is asked first: a `done` row is answered from the file and
//! writes no frame, and a row that stands is run again from where it stands. Only a `local` the file does not hold is
//! prepared; that commit is the point of no return. A row that may run gets the attempt attached ([`Shared::attach`]) and
//! the socket's loop kicked; an open `transmit` row with nobody attached does not run, because the core decides (a message
//! the user deleted while offline never goes). An `acked` row needs nobody: it finishes by itself (`queue::next_send`).
//!
//! ANSWERS COME FROM THE ROWS, AFTER THE ZONE THAT MAKES THEM TRUE. Nothing here says what a frame or an ack did: the socket
//! ends a row's run and this file reads what became durable. Which is why a zone that failed can never be answered `Sent`.
//! The contract, one row per line ([`settled`], [`gone`]):
//!
//! | When | The row | The answer |
//! |---|---|---|
//! | the ledger, or the end of the run | `done`, nobody failed for good | `Sent { server_at: sent_at, per_recipient: the acked }` |
//! | the same | `done`, an `invalid` or a `no-key` receiver | `Partial { delivered: the acked, failed: those, not retryable }` |
//! | the same, under `only` | `reflected`, `only` through, others pending | `Sent { server_at: None, per_recipient: only's acked }` |
//! | the socket is gone, or no ack came | `reflected`, some acked AND some pending | `Partial { delivered, failed: the pending -> Transient }` |
//! | the same | `acked` | `Failed(Transient)`: the row finishes by itself, the retry finds `done` and the real `server_at` |
//! | the same | none acked, or no row | `Failed(Transient)` |
//! | no socket stands past the login-ack | not `done` | `Failed(Transient)`, nothing written |
//! | a `Transmit` of a row that cools | not `done` | `Failed(Transient { retry_after: the time left })`, no frame |
//! | refused before PREPARE (a kind not built, a type the server never acks: `Unsupported`) | none | `Failed`, not retryable (`SendRefused`); a full queue `Transient { 60 s }` |
//!
//! A BLOCKED RECEIVER IS NOT NAMED: the core counts whom it asked for as sent, and the user's own block is no failure.
//!
//! `only` (the contract's "to those and to nobody else"): the attempt writes to the pending ones it names and answers for
//! those it names. A name the row has no receiver for did not get the message and never will on this row, so it is told as
//! `Unsupported`, never left for the core to count as sent. `is_recipient_update` is Signal's and not read.
//!
//! A SECOND `Transmit` of a `local` with a reply attached answers the older one `Transient`.
//!
//! No log line here names an ID, a nonce or a body.

use std::fmt;
use std::time::Duration;

use kourier_backend_api::{OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::ZoneDb;
use kourier_db::rusqlite::Connection;
use kourier_model::{BackendError, Body, ConvKey, ErrorKind, RecipientState, UserId};
use tokio::sync::oneshot;

use super::prepare::{self, NewBody, NewMessage, To};
use super::queue::{self, ReceiverState, SendState, SendTask};
use super::shared::Shared;
use crate::error::{SendRefused, StoreError};
use crate::join::events::{self, SCHEME};
use crate::msg_id;

/// The `kind` column of the core's text.
const KIND_TEXT: &str = "text";
/// How long a `Transmit` waits when its budget is spent (decision C10).
const QUEUE_FULL_WAIT: Duration = Duration::from_secs(60);

const NOT_BUILT: &str = "this kind of message is not sent yet";
const BAD_LOCAL: &str = "the id of a message to send is not readable";
const BAD_ONLY: &str = "a receiver of a message to send is no Threema ID";
const OTHER_CONVERSATION: &str = "the id of a message to send names another conversation";
const NO_SOCKET: &str = "no socket stands past the login";
const NOT_ACKED: &str = "the socket went before the row's ack";
const QUEUE_FULL: &str = "the send queue is full";
const STORE_FAILED: &str = "the send queue's store failed";
const CONTENT_FAILED: &str = "a send row failed by its content";
const REPLACED: &str = "a newer attempt of this message came";
const ROW_GONE: &str = "the send row is gone";
const INVALID: &str = "the receiver's ID is invalid";
const NO_KEY: &str = "the file holds no usable key of the receiver";
const LEFT_OUT: &str = "this message leaves the receiver out";

/// A `Transmit` that waits on its row: whom it asks for and its reply. `Debug` prints counts.
pub(crate) struct Attempt {
    task: i64,
    only: Option<Vec<String>>,
    reply: oneshot::Sender<TransmitOutcome>,
}

impl fmt::Debug for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attempt")
            .field("task", &self.task)
            .field("only", &self.only.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

impl Attempt {
    pub(crate) fn new(
        task: i64,
        only: Option<Vec<String>>,
        reply: oneshot::Sender<TransmitOutcome>,
    ) -> Self {
        Self { task, only, reply }
    }

    /// The row it waits on.
    pub(crate) fn task(&self) -> i64 {
        self.task
    }

    /// Whom it asks for (`None`: everyone).
    pub(crate) fn only(&self) -> Option<&[String]> {
        self.only.as_deref()
    }

    /// The core dropped its end: nothing to tell, and the row stands whatever it was told.
    fn answer(self, outcome: TransmitOutcome) {
        let _ = self.reply.send(outcome);
    }
}

/// What the first look at a `Transmit` came to.
enum Decided {
    /// It is answered, from the rows or as a refusal.
    Answer(TransmitOutcome),
    /// A row may run for it.
    Attach(i64, Option<Vec<String>>),
}

/// THE ACTOR'S `Transmit`, behind the door (module doc).
pub(crate) fn transmit(
    db: &ZoneDb,
    shared: &Shared,
    now_ms: u64,
    req: OutgoingRequest,
    reply: oneshot::Sender<TransmitOutcome>,
) {
    match decide(db, shared, now_ms, req) {
        Decided::Answer(outcome) => {
            let _ = reply.send(outcome);
        }
        Decided::Attach(task, only) => {
            if let Some(older) = shared.attach(Attempt::new(task, only, reply)) {
                older.answer(transient(REPLACED));
            }
            shared.kick();
        }
    }
}

fn decide(db: &ZoneDb, shared: &Shared, now_ms: u64, req: OutgoingRequest) -> Decided {
    let OutgoingRequest {
        conv,
        local,
        sent_at,
        kind,
        only,
        is_recipient_update: _,
    } = req;
    if msg_id::parse(&local).is_none() {
        return refused(ErrorKind::InvalidInput, BAD_LOCAL);
    }
    let Ok(only) = only_of(only) else {
        return refused(ErrorKind::InvalidInput, BAD_ONLY);
    };
    let Some(to) = to_of(&conv) else {
        return refused(ErrorKind::Unsupported, NOT_BUILT);
    };
    let ledger = db.read(|conn: &Connection| {
        let Some(task) = queue::send_ledger(conn, &local)? else {
            return Ok::<_, StoreError>(None);
        };
        let receivers = queue::send_receiver_states(conn, task.id)?;
        Ok(Some((task, receivers)))
    });
    match ledger {
        Ok(Some((task, receivers))) => on_the_ledger(shared, now_ms, &to, &task, &receivers, only),
        Ok(None) => {
            let Some(body) = body_of(kind) else {
                return refused(ErrorKind::Unsupported, NOT_BUILT);
            };
            if !shared.live() {
                return Decided::Answer(transient(NO_SOCKET));
            }
            let message = NewMessage {
                local: local.to_string(),
                kind: KIND_TEXT,
                to,
                created_at: sent_at,
                body,
                specific: Vec::new(),
                only: only.clone(),
            };
            let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
            match prepare::prepare_outgoing(db, &message, now, &shared.attached()) {
                Ok(task) => Decided::Attach(task, only),
                Err(error) => Decided::Answer(failure_of(&error)),
            }
        }
        Err(error) => Decided::Answer(failure_of(&error)),
    }
}

/// A `local` the file holds: answered from the row, or the row runs again from where it stands. The cooling is asked before
/// the attempt is attached, so a row that cools writes nothing.
fn on_the_ledger(
    shared: &Shared,
    now_ms: u64,
    to: &To,
    task: &SendTask,
    receivers: &Receivers,
    only: Option<Vec<String>>,
) -> Decided {
    if task.conversation != prepare::conversation_bytes(to) {
        return refused(ErrorKind::Internal, OTHER_CONVERSATION);
    }
    if let Some(outcome) = settled(task, receivers, only.as_deref()) {
        return Decided::Answer(outcome);
    }
    if let Some(error) = shared.cooling_answer(task.id, now_ms) {
        return Decided::Answer(TransmitOutcome::Failed(error));
    }
    if !shared.live() {
        return Decided::Answer(transient(NO_SOCKET));
    }
    Decided::Attach(task.id, only)
}

/// The run of row `task` ended: its attempt is answered when the row settled it. When it did not, a newer attempt asks for
/// more than this run wrote, and it stays attached: the row runs again for it.
pub(crate) fn row_finished(db: &ZoneDb, shared: &Shared, task: i64) {
    let Some(attempt) = shared.detach(task) else {
        return;
    };
    match read_row(db, task) {
        Ok(Some((row, receivers))) => match settled(&row, &receivers, attempt.only()) {
            Some(outcome) => attempt.answer(outcome),
            None => {
                shared.attach(attempt);
            }
        },
        Ok(None) => attempt.answer(transient(ROW_GONE)),
        Err(error) => attempt.answer(failure_of(&error)),
    }
}

/// Row `task` stopped on this socket and will not go on for now (it cools): its attempt is answered as if the socket went.
pub(crate) fn row_stopped(db: &ZoneDb, shared: &Shared, task: i64) {
    if let Some(attempt) = shared.detach(task) {
        answer_gone(db, attempt);
    }
}

/// The socket is over, or its queue stopped: every attempt is answered. Nothing stays attached past a socket: the core's own
/// retry attaches again, and until then only an `acked` row runs.
pub(crate) fn all_gone(db: &ZoneDb, shared: &Shared) {
    for attempt in shared.detach_all() {
        answer_gone(db, attempt);
    }
}

fn answer_gone(db: &ZoneDb, attempt: Attempt) {
    let outcome = match read_row(db, attempt.task()) {
        Ok(Some((row, receivers))) => gone(&row, &receivers, attempt.only()),
        Ok(None) => transient(ROW_GONE),
        // A row that cannot be read proves nothing, whatever kind of failure: the core comes back.
        Err(_) => transient(STORE_FAILED),
    };
    attempt.answer(outcome);
}

/// A task with its receivers' states: what an answer reads.
type ReadRow = (SendTask, Vec<(String, ReceiverState)>);

/// The row and its receivers' states, for the answers (no box is read: the answers look at states alone).
fn read_row(db: &ZoneDb, task: i64) -> Result<Option<ReadRow>, StoreError> {
    db.read(|conn: &Connection| {
        let Some(row) = queue::send_task(conn, task)? else {
            return Ok(None);
        };
        let receivers = queue::send_receiver_states(conn, task)?;
        Ok(Some((row, receivers)))
    })
}

/// The conversation of a message, as the send core names it. `None`: a chat the send core has no form for yet (a list, a
/// channel, a key of another network).
pub(crate) fn to_of(conv: &ConvKey) -> Option<To> {
    match conv {
        ConvKey::Direct(user) if user.scheme() == SCHEME => {
            Some(To::Contact(user.value().to_owned()))
        }
        ConvKey::Group(key) => {
            events::parse_group_key(key).map(|(creator, group_id)| To::Group { creator, group_id })
        }
        ConvKey::Direct(_) | ConvKey::Broadcast(_) | ConvKey::Channel(_) => None,
    }
}

/// `req.only` as Threema IDs; a user of another network is no receiver here.
fn only_of(only: Option<Vec<UserId>>) -> Result<Option<Vec<String>>, ()> {
    only.map(|users| {
        users
            .iter()
            .map(|user| {
                (user.scheme() == SCHEME)
                    .then(|| user.value().to_owned())
                    .ok_or(())
            })
            .collect()
    })
    .transpose()
}

/// What TH6a sends: a plain text. A quote, a timer and the ranges of mentions and markup have no writer here, and a message
/// that lost them would say something else than the user wrote: refused until a later step writes them.
fn body_of(kind: OutgoingKind) -> Option<NewBody> {
    match kind {
        OutgoingKind::Message {
            body: Body::Text(text),
            reply_to: None,
            expiry: None,
        } if text.ranges.is_empty() => Some(NewBody::Text(text.body)),
        _ => None,
    }
}

fn refused(kind: ErrorKind, detail: &'static str) -> Decided {
    Decided::Answer(TransmitOutcome::Failed(BackendError::new(kind, detail)))
}

fn transient(detail: &'static str) -> TransmitOutcome {
    TransmitOutcome::Failed(BackendError::transient(detail))
}

/// What a failed read or zone tells the core: a refusal for good, a wait for a full queue, a bug for a row that fails by its
/// content; the store's own failure says nothing about the message, so the core comes back.
fn failure_of(error: &StoreError) -> TransmitOutcome {
    TransmitOutcome::Failed(match error {
        StoreError::Refused(reason) => BackendError::new(kind_of(*reason), reason.to_string()),
        StoreError::QueueFull => BackendError::new(
            ErrorKind::Transient {
                retry_after: Some(QUEUE_FULL_WAIT),
            },
            QUEUE_FULL,
        ),
        error if error.is_content() => BackendError::internal(CONTENT_FAILED),
        _ => BackendError::transient(STORE_FAILED),
    })
}

fn kind_of(reason: SendRefused) -> ErrorKind {
    match reason {
        SendRefused::BadLocal
        | SendRefused::UnknownContact
        | SendRefused::UnknownGroup
        | SendRefused::NotAMember
        | SendRefused::BadId => ErrorKind::InvalidInput,
        SendRefused::UnknownType | SendRefused::NotQueued => ErrorKind::Unsupported,
        SendRefused::TooLarge => ErrorKind::TooLarge,
    }
}

/// A task's receivers as the answers read them: the ID and the state, never the box (S-L1).
type Receivers = [(String, ReceiverState)];

/// Who has the message, as the core's answers name them.
type Delivered = Vec<(UserId, RecipientState)>;

/// The receivers an attempt asks for, by what became of them.
#[derive(Default)]
struct Asked {
    /// A durable ack.
    delivered: Delivered,
    /// Still pending: a resend may reach them.
    waiting: Vec<UserId>,
    /// No box for good.
    lost: Vec<(UserId, BackendError)>,
}

impl Asked {
    fn of(receivers: &Receivers, only: Option<&[String]>) -> Self {
        let mut asked = Self::default();
        let named = |identity: &str| only.is_none_or(|names| names.iter().any(|n| n == identity));
        for (identity, state) in receivers.iter().filter(|(identity, _)| named(identity)) {
            // A stored receiver is an ID: its CHECK and the seal hold that. A damaged row is left unnamed.
            let Ok(user) = events::user(identity) else {
                continue;
            };
            match state {
                ReceiverState::Acked => asked
                    .delivered
                    .push((user, RecipientState::Sent { sealed: false })),
                ReceiverState::Pending => asked.waiting.push(user),
                ReceiverState::Invalid => asked.lost.push((
                    user,
                    BackendError::new(ErrorKind::PeerUnregistered, INVALID),
                )),
                ReceiverState::NoKey => asked
                    .lost
                    .push((user, BackendError::new(ErrorKind::Unsupported, NO_KEY))),
                ReceiverState::Omitted => asked
                    .lost
                    .push((user, BackendError::new(ErrorKind::Unsupported, LEFT_OUT))),
                // The user's own block: unnamed, the core counts it as sent (module doc).
                ReceiverState::Blocked => {}
            }
        }
        let has_row = |name: &String| receivers.iter().any(|(identity, _)| identity == name);
        for name in only.unwrap_or_default().iter().filter(|n| !has_row(n)) {
            if let Ok(user) = events::user(name) {
                asked
                    .lost
                    .push((user, BackendError::new(ErrorKind::Unsupported, LEFT_OUT)));
            }
        }
        asked
    }

    /// Who failed: the waiting ones first, each retryable, then those lost for good.
    fn failed(self) -> (Delivered, Vec<(UserId, BackendError)>) {
        let mut failed: Vec<(UserId, BackendError)> = self
            .waiting
            .into_iter()
            .map(|user| (user, BackendError::transient(NOT_ACKED)))
            .collect();
        failed.extend(self.lost);
        (self.delivered, failed)
    }
}

/// `Some`: the rows say all this attempt asked for, and nothing more is owed on its account: the row is `done`, or it is
/// `reflected` with `only` through and others still pending.
fn settled(
    task: &SendTask,
    receivers: &Receivers,
    only: Option<&[String]>,
) -> Option<TransmitOutcome> {
    let asked = Asked::of(receivers, only);
    let server_at = match task.state {
        SendState::Done => task.sent_at,
        SendState::Reflected
            if only.is_some()
                && asked.waiting.is_empty()
                && receivers
                    .iter()
                    .any(|(_, state)| *state == ReceiverState::Pending) =>
        {
            None
        }
        SendState::Prepared | SendState::Reflected | SendState::Acked => return None,
    };
    let (delivered, failed) = asked.failed();
    Some(if failed.is_empty() {
        TransmitOutcome::Sent {
            server_at,
            per_recipient: delivered,
        }
    } else {
        TransmitOutcome::Partial { delivered, failed }
    })
}

/// The answer when the socket went or the run stopped. An `acked` row has nobody pending and nothing the core could say to
/// it: `Failed(Transient)` brings it back, and by then the row has finished by itself (R1). `Partial` with nobody failed
/// would end the core's retries, and the core would never hear `server_at`.
fn gone(task: &SendTask, receivers: &Receivers, only: Option<&[String]>) -> TransmitOutcome {
    if let Some(outcome) = settled(task, receivers, only) {
        return outcome;
    }
    let asked = Asked::of(receivers, only);
    if task.state == SendState::Reflected
        && !asked.delivered.is_empty()
        && !asked.waiting.is_empty()
    {
        let (delivered, failed) = asked.failed();
        return TransmitOutcome::Partial { delivered, failed };
    }
    transient(NOT_ACKED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::send::queue::SendOrigin;

    const ANN: &str = "PEER0001";
    const BOB: &str = "PEER0002";
    const CRE: &str = "PEER0003";
    const DEE: &str = "PEER0004";

    fn task(state: SendState, sent_at: Option<i64>) -> SendTask {
        SendTask {
            id: 7,
            origin: SendOrigin::Transmit,
            kind: KIND_TEXT.to_owned(),
            local: Some("0102030405060708".to_owned()),
            message_id: 1,
            conversation: vec![1],
            created_at: 1,
            state,
            has_update: true,
            sent_at,
            bytes: 0,
            attempts: 0,
            queued_at: 1,
            finished_at: None,
            reflect_env: None,
            update_env: None,
        }
    }

    fn row(identity: &str, state: ReceiverState) -> (String, ReceiverState) {
        (identity.to_owned(), state)
    }

    fn user(identity: &str) -> UserId {
        UserId::parse(&format!("threema:{identity}")).expect("a user")
    }

    fn sent(identity: &str) -> (UserId, RecipientState) {
        (user(identity), RecipientState::Sent { sealed: false })
    }

    fn failed(identity: &str, kind: ErrorKind) -> (UserId, ErrorKind) {
        (user(identity), kind)
    }

    /// Only the kinds count: the details are for a log.
    fn partial(outcome: TransmitOutcome) -> (Delivered, Vec<(UserId, ErrorKind)>) {
        let TransmitOutcome::Partial { delivered, failed } = outcome else {
            panic!("the outcome is not Partial");
        };
        let failed = failed.into_iter().map(|(u, e)| (u, e.kind)).collect();
        (delivered, failed)
    }

    const TRANSIENT: ErrorKind = ErrorKind::Transient { retry_after: None };

    #[track_caller]
    fn is_transient(outcome: TransmitOutcome) {
        let TransmitOutcome::Failed(error) = outcome else {
            panic!("the outcome is not Failed");
        };
        assert_eq!(error.kind, TRANSIENT);
    }

    /// A `done` row with nobody failed: `Sent` at the update's time, the acked named, a blocked one not.
    #[test]
    fn a_done_row_is_sent_at_its_time_and_names_the_acked_alone() {
        let rows = [
            row(ANN, ReceiverState::Acked),
            row(BOB, ReceiverState::Blocked),
            row(CRE, ReceiverState::Acked),
        ];
        assert_eq!(
            settled(&task(SendState::Done, Some(99)), &rows, None),
            Some(TransmitOutcome::Sent {
                server_at: Some(99),
                per_recipient: vec![sent(ANN), sent(CRE)],
            })
        );
    }

    /// A notes group, or everyone blocked: `Sent` with an empty list.
    #[test]
    fn a_done_row_with_nobody_acked_and_nobody_failed_is_sent_to_an_empty_list() {
        for rows in [vec![], vec![row(ANN, ReceiverState::Blocked)]] {
            assert_eq!(
                settled(&task(SendState::Done, Some(5)), &rows, None),
                Some(TransmitOutcome::Sent {
                    server_at: Some(5),
                    per_recipient: Vec::new(),
                })
            );
        }
    }

    /// `done` with an invalid, a no-key and an omitted receiver: `Partial`, none of them retryable.
    #[test]
    fn a_done_row_with_a_receiver_lost_for_good_is_partial_and_not_retryable() {
        let rows = [
            row(ANN, ReceiverState::Acked),
            row(BOB, ReceiverState::Invalid),
            row(CRE, ReceiverState::NoKey),
            row(DEE, ReceiverState::Omitted),
        ];
        let outcome = settled(&task(SendState::Done, Some(99)), &rows, None).expect("settled");
        assert!(
            outcome.retryable_members().is_empty(),
            "a resend is offered"
        );
        assert_eq!(
            partial(outcome),
            (
                vec![sent(ANN)],
                vec![
                    failed(BOB, ErrorKind::PeerUnregistered),
                    failed(CRE, ErrorKind::Unsupported),
                    failed(DEE, ErrorKind::Unsupported),
                ]
            )
        );
    }

    /// Nobody with a box got one: `delivered` is empty, which the core reads as a failed message.
    #[test]
    fn a_done_row_of_receivers_all_lost_delivers_to_nobody() {
        let rows = [row(ANN, ReceiverState::NoKey)];
        let outcome = settled(&task(SendState::Done, Some(99)), &rows, None).expect("settled");
        assert_eq!(
            partial(outcome),
            (Vec::new(), vec![failed(ANN, ErrorKind::Unsupported)])
        );
    }

    /// R4: `only` through and others pending: `Sent` with no time, only's acked named; a row with nobody else pending is not
    /// settled (it goes on to its update), and neither is one whose `only` still waits. No `only`, not settled.
    #[test]
    fn only_through_with_others_pending_is_sent_without_a_time() {
        let rows = [
            row(ANN, ReceiverState::Acked),
            row(BOB, ReceiverState::Acked),
            row(CRE, ReceiverState::Pending),
        ];
        let reflected = task(SendState::Reflected, None);
        let only = [ANN.to_owned()];
        assert_eq!(
            settled(&reflected, &rows, Some(&only)),
            Some(TransmitOutcome::Sent {
                server_at: None,
                per_recipient: vec![sent(ANN)],
            })
        );
        let only = [CRE.to_owned()];
        assert_eq!(settled(&reflected, &rows, Some(&only)), None);
        assert_eq!(settled(&reflected, &rows, None), None);
        let through = [row(ANN, ReceiverState::Acked)];
        let only = [ANN.to_owned()];
        assert_eq!(settled(&reflected, &through, Some(&only)), None);
        assert_eq!(
            settled(&task(SendState::Prepared, None), &rows, Some(&only)),
            None,
            "the reflect's ack is not durable"
        );
        assert_eq!(
            settled(&task(SendState::Acked, None), &through, Some(&only)),
            None,
            "the update is owed"
        );
    }

    /// `only` on a `done` row answers for those it names; a name with no row on it is told, never counted as sent.
    #[test]
    fn only_on_a_done_row_answers_for_those_it_names() {
        let rows = [
            row(ANN, ReceiverState::Acked),
            row(BOB, ReceiverState::Invalid),
        ];
        let done = task(SendState::Done, Some(99));
        let only = [ANN.to_owned()];
        assert_eq!(
            settled(&done, &rows, Some(&only)),
            Some(TransmitOutcome::Sent {
                server_at: Some(99),
                per_recipient: vec![sent(ANN)],
            })
        );
        let only = [ANN.to_owned(), CRE.to_owned()];
        assert_eq!(
            partial(settled(&done, &rows, Some(&only)).expect("settled")),
            (vec![sent(ANN)], vec![failed(CRE, ErrorKind::Unsupported)])
        );
    }

    /// The socket went. Some acked and some pending: `Partial`, the pending retryable, the lost named now (the next attempt's
    /// `only` will not name them). `acked`: `Failed(Transient)`, never a `Partial` with nobody failed. None acked: the same.
    #[test]
    fn when_the_socket_goes_the_row_says_what_is_answered() {
        let rows = [
            row(ANN, ReceiverState::Acked),
            row(BOB, ReceiverState::Pending),
            row(CRE, ReceiverState::NoKey),
        ];
        let reflected = task(SendState::Reflected, None);
        let outcome = gone(&reflected, &rows, None);
        assert_eq!(outcome.retryable_members(), [user(BOB)]);
        assert_eq!(
            partial(outcome),
            (
                vec![sent(ANN)],
                vec![failed(BOB, TRANSIENT), failed(CRE, ErrorKind::Unsupported)]
            )
        );
        is_transient(gone(
            &task(SendState::Acked, None),
            &[row(ANN, ReceiverState::Acked)],
            None,
        ));
        is_transient(gone(&reflected, &[row(BOB, ReceiverState::Pending)], None));
        is_transient(gone(
            &task(SendState::Prepared, None),
            &[row(BOB, ReceiverState::Pending)],
            None,
        ));
        assert_eq!(
            gone(&task(SendState::Done, Some(3)), &rows[..1], None),
            TransmitOutcome::Sent {
                server_at: Some(3),
                per_recipient: vec![sent(ANN)],
            },
            "a row that finished before the socket went"
        );
    }

    /// The same under `only`: an acked receiver the attempt does not name is not this attempt's.
    #[test]
    fn when_the_socket_goes_under_only_the_answer_is_for_those_it_names() {
        let rows = [
            row(ANN, ReceiverState::Acked),
            row(BOB, ReceiverState::Pending),
            row(CRE, ReceiverState::Pending),
        ];
        let reflected = task(SendState::Reflected, None);
        let only = [BOB.to_owned()];
        is_transient(gone(&reflected, &rows, Some(&only)));
        let only = [ANN.to_owned(), BOB.to_owned()];
        assert_eq!(
            partial(gone(&reflected, &rows, Some(&only))),
            (vec![sent(ANN)], vec![failed(BOB, TRANSIENT)])
        );
    }

    /// Each refusal's kind, literal; a full queue waits, a row that fails by its content is a bug, the store's failure says
    /// nothing about the message.
    #[test]
    fn each_failure_of_the_store_is_its_answer() {
        let of = |error: StoreError| {
            let TransmitOutcome::Failed(error) = failure_of(&error) else {
                panic!("the outcome is not Failed");
            };
            error.kind
        };
        for (reason, kind) in [
            (SendRefused::BadLocal, ErrorKind::InvalidInput),
            (SendRefused::UnknownContact, ErrorKind::InvalidInput),
            (SendRefused::UnknownGroup, ErrorKind::InvalidInput),
            (SendRefused::NotAMember, ErrorKind::InvalidInput),
            (SendRefused::BadId, ErrorKind::InvalidInput),
            (SendRefused::UnknownType, ErrorKind::Unsupported),
            (SendRefused::NotQueued, ErrorKind::Unsupported),
            (SendRefused::TooLarge, ErrorKind::TooLarge),
        ] {
            assert_eq!(of(StoreError::Refused(reason)), kind);
        }
        assert_eq!(
            of(StoreError::QueueFull),
            ErrorKind::Transient {
                retry_after: Some(Duration::from_secs(60))
            }
        );
        assert_eq!(of(StoreError::Failed("x")), ErrorKind::Internal);
        assert_eq!(
            of(StoreError::Sql {
                op: "x",
                code: Some(275),
            }),
            ErrorKind::Internal
        );
        assert_eq!(
            of(StoreError::Sql {
                op: "x",
                code: Some(13),
            }),
            TRANSIENT
        );
    }

    /// The conversation's forms: a 1:1 chat and a group by their keys, and no other network's or kind's.
    #[test]
    fn a_chat_is_named_by_its_key_and_no_other_is() {
        assert_eq!(
            to_of(&ConvKey::Direct(user(ANN))),
            Some(To::Contact(ANN.to_owned()))
        );
        let key = events::group_key(CRE, 0x0102_0304_0506_0708).expect("a key");
        assert_eq!(
            to_of(&key),
            Some(To::Group {
                creator: CRE.to_owned(),
                group_id: 0x0102_0304_0506_0708,
            })
        );
        let other = UserId::parse("aci:00000000-0000-0000-0000-000000000000").expect("a user");
        assert_eq!(to_of(&ConvKey::Direct(other)), None);
        assert_eq!(to_of(&ConvKey::Broadcast("threema:x".into())), None);
        assert_eq!(to_of(&ConvKey::Channel("threema:x".into())), None);
        assert_eq!(to_of(&ConvKey::Group("signal:abc".into())), None);
    }

    /// Only a plain text has a body here.
    #[test]
    fn only_a_plain_text_has_a_body() {
        let plain = OutgoingKind::Message {
            body: Body::text("synthetic"),
            reply_to: None,
            expiry: None,
        };
        assert_eq!(body_of(plain), Some(NewBody::Text("synthetic".to_owned())));
        let timed = OutgoingKind::Message {
            body: Body::text("synthetic"),
            reply_to: None,
            expiry: Some(Duration::from_secs(60)),
        };
        assert_eq!(body_of(timed), None, "a timer");
        let mut ranged = kourier_model::Text::plain("synthetic");
        ranged.ranges.push(kourier_model::TextRange {
            start: 0,
            len: 3,
            kind: kourier_model::RangeKind::Bold,
        });
        let marked = OutgoingKind::Message {
            body: Body::Text(ranged),
            reply_to: None,
            expiry: None,
        };
        assert_eq!(body_of(marked), None, "markup");
    }

    /// `Debug` of an attempt prints counts.
    #[test]
    fn an_attempt_prints_counts_only() {
        let (reply, _answer) = oneshot::channel();
        let attempt = Attempt::new(7, Some(vec![ANN.to_owned()]), reply);
        let printed = format!("{attempt:?}");
        assert!(!printed.contains(ANN), "an ID in a Debug form");
        assert!(printed.contains("only: Some(1)"), "the count is missing");
    }
}
