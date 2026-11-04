//! One `Transmit` (W5a, chunk 9a; decisions D2, D4, D5, D6, D7; brief §2.2-2.3): what the actor runs for each
//! attempt, off its loop (the wait for the ack takes up to [`ACK_WAIT`]).
//!
//! THE ORDER, every step before the next:
//! 1. THE LEDGER FIRST (D4): an outcome recorded for this local is the answer, with NO frame: `acked` ->
//!    `Sent{server_at}`, `refused` -> `Failed(ReachOutRefused)` (a 463 is never met twice through Kourier).
//! 2. D6 IN THE BACKEND (D7): a direct chat the phone never started is refused with no frame,
//!    `Failed(Unsupported)` "start this chat on your phone" (never `ReachOutRefused`: no Send Again trap). A group
//!    is always started (the server owns its membership).
//!    THE EDIT SLACK RULE (D9, critic M3; architecture §13 phase H): receivers drop an edit whose own time is
//!    [`EDIT_RECEIVER_WINDOW_SECS`] or more after its original's (upstream's `EDIT_PROCESSING_WINDOW_SECS`; WA
//!    Web's rule for plain edits is U4). The original's age is judged at send time by the ledger's server time
//!    for an original Kourier sent: an edit sent later than [`EDIT_SEND_LIMIT_SECS`] after it is
//!    `Failed(Expired)`, no frame (the core rolls it back). For an original whose time the backend does not know
//!    (one from the phone: W3 keeps no arrival time) a PROXY judges instead: the edit waited in the outbox more
//!    than [`EDIT_WAIT_PROXY`] since it was made (the stated edit window is 900 s of the receivers' 1,200 s, and 120 s of
//!    the rest are kept for the clocks and the way to the server like the known-original limit: 180 s, fix F15).
//!    A revoke and a reaction have no such rule (no receiver window is known: U4).
//!    THE HOLD (D8, chunk 10): while the reach-out lock of this run is unknown (the first pull has not answered,
//!    for at most 10 minutes since it was asked) or a hold is known, nothing is written: `Transient{retry_after:
//!    Some(HOLD_RETRY)}` (the actor's `send_gate` decides, [`Gate`]). The gate is asked AGAIN right before the frame
//!    is written (fix F6, `send::hold::Recheck`): a lock pushed while the attempt awaited the store, the account's
//!    mode or a group's addressing stops this frame too. Receipts are not held.
//! 3. Our own number must be known (critic H4): before it is, the answer is transient. A session must be there.
//!    THE FILE WORK (W5b, G1-G5; [`file_work`]): only now, after every gate (nothing is read, re-encoded or uploaded
//!    for a message that will be refused), and only for a new message with ONE file (more is refused by the build,
//!    G10). First the refusals of the build that the pointer does not decide (a quote naming another chat, a quoted
//!    author that does not parse) are judged by the build's own function ([`content::check_file`], fix F4), with the
//!    account's mode and the group's asked once for it and for the build. Then: the store read's upload record of this
//!    local, younger than 6 hours, is sent again with no upload (reported whole at once); an earlier POST of this local
//!    that still runs on its blocking thread answers `Transient{STALL_RETRY}` with no work (fix F5); else
//!    `Preparing`, the core's file prepared into the sealed temporary file, the hold asked again (fix F3), the file
//!    uploaded under the person's stop and the stall watch, and its pointer committed to the upload record in ONE zone
//!    (kill points `wa_inside/after_media_upload`) BEFORE the build and the frame. The temporary file goes with the
//!    upload. A stop the person was told `Stopping` for makes every way out of this part (the file work, the build,
//!    the hold's re-check) answer as a stop, never `Transient` (fix F1: the core resends a transient answer). A new
//!    message with a file that ends for good before its frame (stopped, or refused by the build) deletes its own
//!    upload record before it answers (fix F13); a transient answer keeps it for the resend.
//! 4. The message is built (`send::content`, pure) with the account's namespace, the store's PN <-> LID pairs and a
//!    group's addressing mode. In a group whose mode is LID or unknown a person the core holds by NUMBER (and the
//!    mapping gives no LID for) is never written: the mention is left out and a quote's participant stays out, so
//!    no member reads a number (fix F7; logged by kind). A KEY cannot leave its person out (a reaction to a member's
//!    message): in a LID group the send fails for good (`Unsupported`), in a group whose mode could not be read it is
//!    `Transient` (the mode is asked again at the next send), and no frame goes out (fix F20).
//!    A delete for me makes no frame: `Sent` with nothing sent, logged as a FAULT (D9: the
//!    features never offer it). Any other request that makes no message fails for good, with no frame.
//!    THE POINT OF NO RETURN (W5b, G1, G2; `send::stop`): after the hold's re-check, `stop.close()`; an abort (the X on
//!    an upload, only a new message with files can have one) that came first ends the attempt there, `Failed(Internal,
//!    "stopped by the user before anything was sent")`, no frame; any abort after it is too late. The app's QUIT is the
//!    same race (fix F16): an attempt the quit took before this point answers `Transient` ([`stop::quitting`]: the core
//!    sends the message again at the next start; its upload record stays), every await of the file work ends with it,
//!    and the quit does not wait for it; one that passed first is waited for (W5a D19).
//! 5. THE REGISTRY (D5): the wait is registered BEFORE the frame is written. A local whose frame was written on
//!    the live connection and is not answered yet writes NOTHING: it waits for that frame's answer. A failure
//!    before the wire withdraws the wait (no frame left); only a missing device of the recipient is tried ONCE
//!    more with the list asked again (`refresh_first`); a socket that failed while writing counts as written (the
//!    frame may have left): its wait ends with the connection.
//! 6. THE ANSWER: an ack, a 463 or a 475 (fix F8: met like a 463) is committed to the ledger in ONE zone (`ledger::commit`, kill points
//!    `wa_inside_send_outcome` / `wa_after_send_outcome`) BEFORE the reply leaves; any other nack is not recorded
//!    (critic H2) and fails by the one table (`send::errors`), the retryable ones with their FLOOR as
//!    `retry_after` (fix F1): the registry remembers the nack, so a Transmit of that local inside the floor writes
//!    NO frame and answers the same failure with the time left (the core's own retry cannot make a burst); after
//!    the floor ONE new frame goes out (after a 421 it asks for fresh group metadata). The connection ended:
//!    `Transient{retry_after: None}`, and the core resends under the SAME id (his answer: "Resend, same id").
//! 7. NO ANSWER WITHIN [`ACK_WAIT`] (D5, chunk 9b): the reply is `Transient{retry_after: None}` at once, and the
//!    attempt goes on AFTER it: the connection is ended (the first such wait of the connection claims it:
//!    `Session::drop_connection`, then the registry's end, so every wait of it answers transient and the core's
//!    resend can go out only on a later connection, with the same id); an ack that still comes before the end is
//!    recorded in the ledger (the late ack), so the resend is answered from the ledger with no frame.
//!
//! THE TWO COUNT LINES (T2 F1, the run tool's eyes on the wire): [`write_once`] is the ONE place a `Transmit` attempt (and
//! the timer set) writes its frame and judges its answer, and it says so in two INFO lines of fixed words, with a kind
//! and a number and nothing else (no id, no local, no chat, no text): `a send frame was written kind=<message|reaction|
//! edit|revoke|timer|other>` right after the write succeeded (a socket that failed WHILE writing counts as written: the
//! frame may have left), and `a send frame was answered outcome=<acked|refused|nacked|no-ack|connection-ended>
//! code=<number, 0 for none> again=<no|seconds>` when that frame's wait ends. `again` says whether the core sends the
//! local again: `no` when the answer is final for the core (acked, 463/475, 404, 400, 479, any other or unreadable
//! code, and every answer to a timer set), else the seconds it waits at least (the floor the error carries: 429 five
//! minutes, 421/500/503 thirty seconds; 0 for `no-ack` and `connection-ended`). It is read off the SAME error the
//! attempt gives the core ([`answered_as`]), so there is no second table of codes (T2 G1). A second Transmit of a local in the air writes no frame and says
//! nothing (the first one's lines stand for it); a failure before the wire says nothing. Receipts, app-state writes and
//! upstream's own stanzas do not pass here and get no line. `xtask/src/wa_run.rs` counts these lines (and the fake that
//! plays WhatsApp prints the same two, `kourier-fake` `rehearsal.rs`): reword one and its tests fail there.
//!
//! [`transmit`] replies through the core's oneshot and returns when the attempt is over (step 7 included): the actor
//! holds it in its `attempts`, and a normal quit waits for them within its grace (D19).
//!
//!    THE TOKEN GATE (D12, chunk 13; `send::tokens`): a direct chat with no stored token waits, `Transient{retry_after:
//!    Some(TOKEN_RETRY)}`, while the link's history push may still bring the token, at most 30 minutes after the link.
//!
//! Nothing here logs an id, a chat, a person or a text: a request's kind and a reason only.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{Envelope, OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_model::{
    Attachment, BackendError, Body, ConvKey, ErrorKind, RangeKind, TargetAuthor, Text, UserId,
};
use tokio::sync::mpsc;
use wacore_binary::Jid;

use super::acks::{Acks, Answer, Nack, Wait, Waited};
use super::address::{Addressing, Unaddressable, jid_of};
use super::content::{self, Unbuildable};
use super::errors::{Cause, error_of, nack_error, refresh_first};
use super::hold::Recheck;
use super::known::{self, Known};
use super::ledger::{self, Entry, Kind, Outcome};
use super::stop::{self, Registered};
use super::upload::{self, Halt, Lingering, Reports};
use super::{record, started, tokens};
use crate::map::OwnIds;
use crate::media::MediaPointer;
use crate::session::{Devices, Groups, Outgoing, SendFailure, Session};
use crate::store::KourierStore;
use crate::store::people;

/// How long a written frame waits for the server's ack (whatsmeow's default ack timeout, wm/`send.go:154-155`).
#[cfg(not(test))]
pub(crate) const ACK_WAIT: Duration = Duration::from_secs(75);
#[cfg(test)]
pub(crate) const ACK_WAIT: Duration = Duration::from_secs(3);

/// The words of the D7 refusal (they reach the person through the core's failure text).
pub(crate) const START_ON_PHONE: &str = "start this chat on your phone";

/// Receivers drop an edit whose own time is this many seconds or more after its original's (upstream's
/// `EDIT_PROCESSING_WINDOW_SECS`, `wacore/src/msg_secret.rs:126`, checked at `src/message/msg_secret.rs:405`).
pub(crate) const EDIT_RECEIVER_WINDOW_SECS: i64 = 1200;

/// The latest an edit is sent after its original's server time: the receivers' window less 120 s for our clock
/// against the server's and the edit's way to it.
pub(crate) const EDIT_SEND_LIMIT_SECS: i64 = EDIT_RECEIVER_WINDOW_SECS - 120;

/// A PROXY, used only when the original's time is unknown (an original from the phone): an edit that waited longer
/// than this since it was made may land past the receivers' window. An edit is made at most 900 s after its original
/// (the window `features.rs` states), so it may wait the rest of the receivers' 1,200 s less the same 120 s margin as
/// [`EDIT_SEND_LIMIT_SECS`]: 1,200 - 900 - 120 = 180 s (fix F15; 300 s left no margin at the receivers' boundary).
pub(crate) const EDIT_WAIT_PROXY: Duration = Duration::from_secs(180);

/// What one attempt needs from the actor.
pub(crate) struct Parts {
    pub(crate) store: KourierStore,
    /// The running client, if any.
    pub(crate) session: Option<Arc<dyn Session>>,
    /// The account's ack registry.
    pub(crate) acks: Arc<Acks>,
    /// Our own ids as the actor knows them.
    pub(crate) own: OwnIds,
    /// The actor's clock now (ms): the ledger row's time.
    pub(crate) now_ms: i64,
    /// The reach-out lock as the actor knows it: asked at the start (D8) and AGAIN right before the frame is
    /// written (fix F6: a lock pushed meanwhile stops this frame too).
    pub(crate) hold: Recheck,
    /// When this run linked the account, if it did (the token gate, D12).
    pub(crate) linked_ms: Option<i64>,
    /// The attempt's place in the actor's `Transmits` (W5b, G2): its stop state, registered before the attempt was
    /// spawned; the entry goes when this guard is dropped with the attempt.
    pub(crate) stop: Registered,
    /// What the file work needs (W5b): only a new message with a file uses it.
    pub(crate) files: Files,
}

/// What the file work of an attempt needs from the actor (W5b, B11).
pub(crate) struct Files {
    /// The account's event channel: the upload's reports (ephemeral) go there.
    pub(crate) events: mpsc::Sender<Envelope>,
    /// The core's blob directory (`AccountContext::blob_dir`): an attachment's file is found inside it only.
    pub(crate) blob_dir: PathBuf,
    /// The account's `upload-tmp` directory (`send::tmp::dir_beside`): the sealed temporary file goes there.
    pub(crate) tmp_dir: PathBuf,
    /// The POSTs of the account that may still run on a blocking thread after their attempt answered (fix F5).
    pub(crate) lingering: Lingering,
}

#[cfg(test)]
impl Files {
    /// No file work (an attempt of a text, an edit, a delete, a reaction): the reports go nowhere, no directory.
    pub(crate) fn none() -> Self {
        let (events, _) = mpsc::channel(1);
        Self {
            events,
            blob_dir: PathBuf::new(),
            tmp_dir: PathBuf::new(),
            lingering: Lingering::default(),
        }
    }
}

/// What the reach-out lock lets a send do (D8; the actor's `send_gate`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// No hold is known (or "unknown" has ended: 10 minutes of a pull that does not answer).
    Open,
    /// The first pull of the run has not answered yet: a send into an unknown lock can earn a 463 that fails the
    /// chat for good.
    Unknown,
    /// A hold is known (the core's own dispatcher gate missed it in a race).
    Held,
}

/// What a send answers while the lock holds it back (D8): the core tries again after this.
pub(crate) const HOLD_RETRY: Duration = Duration::from_secs(30);

/// What the store says before anything is built (one read job).
struct Looked {
    outcome: Option<Outcome>,
    started: bool,
    known: Known,
    /// For an edit: the server time (s) of its original, when the ledger has it.
    original_t: Option<i64>,
    /// A delete for everyone in a group: the group's row says we are an admin there and still in it (the admin revoke
    /// of another member's message, W6a2 B7b). Always false for anything else; the build gates on it.
    admin: bool,
    /// The admin revoke's target author in the form his message arrived under, when the inbox ledger still has it (fix F3,
    /// `people::received_as`); the revoke's key names him so. `None` for anything else, or when the ledger has no row.
    received_as: Option<UserId>,
    /// What the token gate reads (D12).
    tokens: tokens::Facts,
    /// The upload record (W5b, G4; `send::record`): a file message's own (a resend sends it again), a caption
    /// edit's target's.
    record: Option<record::Stored>,
}

/// Runs the attempt of `req`: its answer goes to `reply` (after the outcome is durable), then what is left of it
/// after an ack that did not come in time (step 7) runs before this returns.
pub(crate) async fn transmit(
    parts: Parts,
    req: OutgoingRequest,
    reply: tokio::sync::oneshot::Sender<TransmitOutcome>,
) {
    let (outcome, late) = attempt(parts, req).await;
    // A core that stopped waiting reads its dropped reply as transient: nothing to do here.
    let _ = reply.send(outcome);
    if let Some(late) = late {
        late.listen().await;
    }
}

/// The attempt up to its answer, and the step-7 rest when the ack did not come in time.
async fn attempt(parts: Parts, req: OutgoingRequest) -> (TransmitOutcome, Option<Late>) {
    let what = req.kind.name();
    let (kind, target) = ledger_kind(&req.kind);
    let conv = req.conv.clone();
    let local = req.local.to_string();
    let users = users_of(&req);
    let record_key = record::key_of(&req);
    let original = match &req.kind {
        OutgoingKind::Edit { target, .. } => Some(target.local.to_string()),
        _ => None,
    };
    // Only a delete for everyone in a group looks at the group's row (the admin revoke asks it).
    let wants_admin = matches!(
        (&req.kind, &conv),
        (
            OutgoingKind::Delete {
                for_everyone: true,
                ..
            },
            ConvKey::Group(_)
        )
    );
    // The author of the message an admin revoke deletes, and its id (fix F3).
    let revoked = match &req.kind {
        OutgoingKind::Delete {
            target,
            for_everyone: true,
        } => match &target.author {
            TargetAuthor::User(author) => Some((author.clone(), target.local.to_string())),
            _ => None,
        },
        _ => None,
    };
    let looked = {
        let conv = conv.clone();
        let local = local.clone();
        parts
            .store
            .read(move |conn| {
                let admin = wants_admin && people::is_admin(conn, &conv)?;
                Ok(Looked {
                    outcome: ledger::look(conn, &local)?,
                    started: started::holds(conn, &conv)?,
                    known: known::load(conn, users.iter())?,
                    original_t: match &original {
                        Some(original) => ledger::acked_at(conn, original)?,
                        None => None,
                    },
                    admin,
                    received_as: match (&revoked, admin) {
                        (Some((author, target_local)), true) => {
                            people::received_as(conn, &conv, target_local, author)?
                        }
                        _ => None,
                    },
                    tokens: tokens::facts(conn, &conv)?,
                    record: match &record_key {
                        Some(key) => record::look(conn, key)?,
                        None => None,
                    },
                })
            })
            .await
    };
    let looked = match looked {
        Ok(looked) => looked,
        Err(e) => {
            tracing::warn!(kind = what, error = %e, "a send could not read its store");
            return (
                TransmitOutcome::Failed(BackendError::transient("the store could not be read")),
                None,
            );
        }
    };
    // 1. The ledger first: what the server answered once is the answer, with no frame.
    match looked.outcome {
        Some(Outcome::Acked { server_t }) => return (sent(server_t), None),
        Some(Outcome::Refused) => return (TransmitOutcome::Failed(refused()), None),
        None => {}
    }
    // A delete for me is never a frame (D9), whatever the chat.
    if matches!(
        req.kind,
        OutgoingKind::Delete {
            for_everyone: false,
            ..
        }
    ) {
        tracing::error!(
            kind = what,
            "FAULT: a delete for me reached the WhatsApp backend; nothing was sent"
        );
        return (sent(None), None);
    }
    // 2. D6 in the backend (D7).
    if !looked.started {
        tracing::info!(
            kind = what,
            "a send into a chat the phone has not started was refused"
        );
        return (
            TransmitOutcome::Failed(BackendError::unsupported(START_ON_PHONE)),
            None,
        );
    }
    // The edit slack rule (D9).
    if matches!(req.kind, OutgoingKind::Edit { .. })
        && edit_too_late(looked.original_t, req.sent_at, parts.now_ms)
    {
        tracing::info!(
            kind = what,
            judged_by = if looked.original_t.is_some() {
                "the original's time"
            } else {
                "the proxy"
            },
            "an edit too late for the receivers' window was not sent"
        );
        return (
            TransmitOutcome::Failed(BackendError::new(
                ErrorKind::Expired,
                "the edit came too late for the recipients to take it",
            )),
            None,
        );
    }
    // THE HOLD (D8): nothing is written while the lock is unknown or a hold is known.
    if let Some(answer) = waits_for_the_lock(what, parts.hold.gate()) {
        return (answer, None);
    }
    // THE TOKEN GATE (D12): the history may still bring this chat's token.
    if tokens::decide(
        matches!(conv, ConvKey::Direct(_)),
        looked.tokens,
        parts.linked_ms,
        parts.now_ms,
    ) {
        tracing::info!(
            kind = what,
            "a send waits for the chat's token from the history"
        );
        return (
            TransmitOutcome::Failed(BackendError::new(
                ErrorKind::Transient {
                    retry_after: Some(tokens::TOKEN_RETRY),
                },
                "the chat's history has not arrived yet",
            )),
            None,
        );
    }
    // 3. Our own number (critic H4).
    if parts.own.pn.is_none() || parts.own.lid.is_none() {
        return (
            TransmitOutcome::Failed(BackendError::transient(
                "the account's own ids are not known yet",
            )),
            None,
        );
    }
    let Some(session) = parts.session.clone() else {
        return (
            TransmitOutcome::Failed(error_of(Cause::Failure(SendFailure::NoClient))),
            None,
        );
    };
    if req.only.is_some() {
        // A group stanza is one for every member (sender keys): W5a never answers `Partial`, so this never
        // happens; it is sent whole.
        tracing::warn!(
            kind = what,
            "a send to some members only is sent to every member"
        );
    }
    // 4. The message.
    let to = match jid_of(&conv) {
        Ok(to) => to,
        Err(reason) => {
            return (
                TransmitOutcome::Failed(BackendError::unsupported(format!(
                    "this chat has no address on WhatsApp ({reason:?})"
                ))),
                None,
            );
        }
    };
    // 3b-4. Everything before the frame, up to the hold's re-check (below). A stop the person asked for wins over any other
    // way this part ends (fix F1): a message they were told is `Stopping` never answers `Transient`, which the core would
    // resend.
    let out = match before_the_frame(&parts, &*session, &req, &looked, to, &local, what).await {
        Ok(out) => out,
        Err(outcome) => {
            let outcome = stopped_if_asked(&parts, outcome);
            forget_a_dead_record(&parts, &req, &local, what, &outcome).await;
            return (outcome, None);
        }
    };
    // THE POINT OF NO RETURN (W5b, G1): after the hold's re-check and right before the frame. An abort that won the
    // race (the X) ends the attempt here with nothing written; from here on an abort is too late.
    if !parts.stop.stop().close() {
        // The person's X won, or the app's quit did (fix F16): the quit's answer is `Transient`, never `stopped()` (which
        // the core shows as failed for good); its upload record stays for the resend.
        let outcome = if parts.stop.stop().is_quitting() {
            tracing::info!(kind = what, "a send was ended by the quit before its frame");
            TransmitOutcome::Failed(stop::quitting())
        } else {
            tracing::info!(
                kind = what,
                "a send was stopped by the user before its frame"
            );
            TransmitOutcome::Failed(stop::stopped())
        };
        forget_a_dead_record(&parts, &req, &local, what, &outcome).await;
        return (outcome, None);
    }
    // 5. The registry, then the frame.
    let answer = match write_once(&*session, &parts.acks, out, frame_kind(&req.kind)).await {
        Ok(Written::Waited(waited, left)) => (waited, left),
        Ok(Written::TimedOut(wait)) => {
            // 7. No ack in time: transient now, the connection's end and the late ack after the reply.
            let late = Late {
                wait,
                what,
                session,
                acks: parts.acks,
                store: parts.store,
                now_ms: parts.now_ms,
                conv,
                local,
                kind,
                target,
            };
            return (TransmitOutcome::Failed(no_ack()), Some(late));
        }
        Err(error) => return (TransmitOutcome::Failed(error), None),
    };
    // 6. The answer: recorded before the reply.
    let (answer, left) = answer;
    let outcome = match answer {
        Waited::Answered(answer) => {
            let Some(outcome) = answer.recorded() else {
                let Answer::Nacked(nack) = answer else {
                    unreachable!("an ack is always recorded")
                };
                // A remembered nack says what is left of its floor, a live one the whole floor (fix F1).
                let retry_after = left.or_else(|| nack.floor());
                return (TransmitOutcome::Failed(nack_error(nack, retry_after)), None);
            };
            let entry = Entry {
                conv,
                local,
                kind,
                target,
                outcome,
            };
            match ledger::commit(&parts.store, entry, parts.now_ms).await {
                Ok(_) => match outcome {
                    Outcome::Acked { server_t } => sent(server_t),
                    Outcome::Refused => TransmitOutcome::Failed(refused()),
                },
                Err(e) => {
                    // The server has it, the ledger does not: the core's resend on this connection meets the
                    // registry's memory of the answer (no frame) and commits again; on a later connection it is
                    // the server's dedupe of the same id (U1, his answer).
                    tracing::error!(kind = what, error = %e, "a send's outcome could not be recorded");
                    TransmitOutcome::Failed(BackendError::transient(
                        "the send's outcome could not be recorded",
                    ))
                }
            }
        }
        Waited::Ended => TransmitOutcome::Failed(connection_ended()),
    };
    (outcome, None)
}

/// The failure of a frame nobody answered within [`ACK_WAIT`] (step 7). ONE constructor for the attempt, the timer set
/// and the second count line's `again` (T2 G1): the line is read off the very error the core is given.
pub(super) fn no_ack() -> BackendError {
    BackendError::transient("the server did not answer in time")
}

/// The failure of a frame whose connection ended before the server answered (same ONE constructor rule as [`no_ack`]).
pub(super) fn connection_ended() -> BackendError {
    BackendError::transient("the connection ended before the server answered")
}

/// A stop the person asked for wins (fix F1, robustness HIGH): the attempt's `Failed` answer before its point of no
/// return becomes [`stop::stopped`] when an abort was accepted (`Stopping` was the answer to the X). Any other
/// answer (`Sent`, a failure after a stop was not asked) stays. The state is still open or requested here, never
/// closed: `close()` comes after.
fn stopped_if_asked(parts: &Parts, outcome: TransmitOutcome) -> TransmitOutcome {
    match outcome {
        TransmitOutcome::Failed(_) if parts.stop.stop().is_requested() => {
            TransmitOutcome::Failed(stop::stopped())
        }
        other => other,
    }
}

/// A new message with a file that ends for good before its frame (stopped by the person, or refused by the build) has no
/// use for its upload record, whose media key, paths and preview would stay for the 30 days of the sweep (fix F13).
/// The attempt deletes its own row in one zone before it answers; a failed delete is logged by kind and left to the
/// sweep. A transient answer keeps the row (the resend within 6 hours sends the same upload again), and so does
/// every request that is not a new message with a file: a caption edit reads its TARGET's row and never owns one.
async fn forget_a_dead_record(
    parts: &Parts,
    req: &OutgoingRequest,
    local: &str,
    what: &'static str,
    outcome: &TransmitOutcome,
) {
    let TransmitOutcome::Failed(error) = outcome else {
        return;
    };
    let new_file_message = matches!(
        &req.kind,
        OutgoingKind::Message {
            body: Body::Media { .. },
            ..
        }
    );
    if error.is_retryable() || !new_file_message {
        return;
    }
    if let Err(e) = record::discard(&parts.store, local.to_owned()).await {
        tracing::warn!(
            kind = what,
            error = %e,
            "the upload record of a message that will not be sent could not be deleted"
        );
    }
}

/// Steps 3b and 4 up to the hold's re-check: the account's mode and the group's (asked once, before the file work), the
/// refusals of a file message that its pointer does not decide (fix F4: nothing of the file is read, uploaded or
/// asked of the CDN for a message that will be refused), THE FILE WORK, the build and the hold asked again (fix F6 of
/// W5a). `Err` is the attempt's answer; no frame was written on any of these ways out.
async fn before_the_frame(
    parts: &Parts,
    session: &dyn Session,
    req: &OutgoingRequest,
    looked: &Looked,
    to: Jid,
    local: &str,
    what: &'static str,
) -> Result<Outgoing, TransmitOutcome> {
    let conv = &req.conv;
    let migrated = session.lid_migrated().await;
    let group = match conv {
        ConvKey::Group(_) => session.group_mode(to.clone()).await.ok(),
        _ => None,
    };
    // 3b. THE FILE WORK (W5b, G1): after every gate, before the build. A message with ONE file (more is refused by the
    // build, G10) has it uploaded, or its upload record sent again within 6 hours; its record is durable before the
    // frame.
    let file = match &req.kind {
        OutgoingKind::Message {
            body: Body::Media { attachments, .. },
            ..
        } if attachments.len() == 1 => {
            // The build's own refusals that the pointer does not decide come first (fix F4), by the build's function.
            let checked = {
                let addressing = Addressing {
                    migrated,
                    map: &looked.known,
                    own: &parts.own,
                };
                content::check_file(req, &addressing, group)
                    .map_err(|reason| unbuildable(what, reason))?
            };
            let job = FileJob {
                what,
                conv,
                local,
                attachment: &attachments[0],
                captioned: checked.captioned,
                stored: looked.record.clone(),
            };
            match file_work(parts, session, job).await {
                Ok(pointer) => Some(pointer),
                Err(error) => {
                    tracing::info!(
                        kind = what,
                        error = error.kind.name(),
                        "a file was not sent"
                    );
                    return Err(TransmitOutcome::Failed(error));
                }
            }
        }
        _ => None,
    };
    // The addressing borrows the pairs; it lives only for the build (no await meanwhile).
    let built = {
        let addressing = Addressing {
            migrated,
            map: &looked.known,
            own: &parts.own,
        };
        // A caption edit of a file Kourier sent writes it from the target's upload record (W5b B10; the edit window is
        // far inside the record's 30 days). A file message's pointer is the file work's.
        let file = match &req.kind {
            OutgoingKind::Edit { .. } => looked.record.as_ref().map(|r| &r.pointer),
            _ => file.as_ref(),
        };
        content::build(
            req,
            &addressing,
            group,
            file,
            looked.admin,
            looked.received_as.as_ref(),
        )
    };
    let built = built.map_err(|reason| unbuildable(what, reason))?;
    if built.plain {
        tracing::info!(
            kind = what,
            "formatting the markup cannot carry was left out"
        );
    }
    if built.withheld {
        tracing::info!(
            kind = what,
            "a person's number was left out of a group message that must not carry it"
        );
    }
    let out = Outgoing {
        to,
        id: local.to_owned(),
        message: built.message,
        expiration: built.expiration,
        devices: Devices::Cached,
        groups: Groups::Cached,
    };
    // THE HOLD AGAIN (fix F6): the store, the account's mode and a group's addressing were awaited since the first
    // look; a lock pushed meanwhile (earned by another frame) must stop this frame, whose wait is not registered yet.
    if let Some(answer) = waits_for_the_lock(what, parts.hold.gate()) {
        return Err(answer);
    }
    Ok(out)
}

/// What the file work of one message needs to know of it.
struct FileJob<'a> {
    /// The request's kind (a log word).
    what: &'static str,
    conv: &'a ConvKey,
    local: &'a str,
    attachment: &'a Attachment,
    /// The RENDERED caption is not empty ([`content::check_file`]): the same fact the build writes the message from.
    captioned: bool,
    /// The store's upload record of this local, if it has one.
    stored: Option<record::Stored>,
}

/// THE FILE WORK of a message with one file (W5b, brief §2.1 step 3, §2.2; decisions G1-G5): the pointer the message
/// is built from.
///
/// - An upload record of this local younger than [`record::REUSE_MS`] (a resend after the record was committed) is
///   sent again: no upload, reported whole at once (the contract's "pointer reused").
/// - An earlier POST of this local that still runs on its blocking thread ([`upload::Lingering`], fix F5) answers
///   `Transient{STALL_RETRY}` at once: no prepare, no temporary file, no `w:m` IQ.
/// - Else `Preparing` is reported (the X from the first moment), the core's file is PREPARED on the blocking pool
///   into the sealed temporary file ([`upload::prepare`]), the hold is asked again (fix F3: a lock earned meanwhile
///   by another frame keeps the account from talking to the CDN for this message), the file UPLOADED
///   ([`upload::send_file`]: the reports, the stall watch, the person's stop), and its pointer committed to the
///   upload record in ONE zone (kill points `wa_inside/after_media_upload`) BEFORE the frame. The temporary file goes
///   as soon as the upload answered (and with the attempt on every other way out: the last holder of the file removes
///   it).
///
/// Every await before the record zone runs under the person's stop (the blocking reads see it through [`Halt`]);
/// a stop that lands while the zone runs is honoured after the commit, at the point of no return (the record stays
/// for its sweep). A record that could not be written fails the attempt `Transient` with no frame: the resend
/// uploads again (the first upload is an orphan on the CDN).
async fn file_work(
    parts: &Parts,
    session: &dyn Session,
    job: FileJob<'_>,
) -> Result<MediaPointer, BackendError> {
    let FileJob {
        what,
        conv,
        local,
        attachment,
        captioned,
        stored,
    } = job;
    let reports = Reports::new(parts.files.events.clone(), parts.stop.local());
    if let Some(stored) = stored.filter(|stored| stored.reusable(parts.now_ms)) {
        tracing::info!("a resend sends its file's upload again");
        reports.full(stored.pointer.file_length).await;
        return Ok(stored.pointer);
    }
    if parts.files.lingering.holds(parts.stop.local()) {
        tracing::info!(
            kind = what,
            "an earlier upload of this message still runs; it waits"
        );
        return Err(BackendError::new(
            ErrorKind::Transient {
                retry_after: Some(upload::STALL_RETRY),
            },
            "an earlier upload of this message is still running",
        ));
    }
    reports.preparing().await;
    let stop = parts.stop.stop().clone();
    let halt = Halt::new(stop.clone()).lingering(parts.files.lingering.clone(), parts.stop.local());
    // Whichever way the file work ends, a blocking read still running (the prepare, a POST) fails at its next call.
    let _ends = halt.ends_with_guard();
    // A still picture takes the process's one picture turn first (fix F17), under the stop; the permit goes into the
    // blocking prepare and is released when that thread ends.
    let turn = stop
        .until_stopped(async { Ok(upload::picture_turn(attachment).await) })
        .await?;
    let job = {
        let (blob_dir, tmp_dir) = (parts.files.blob_dir.clone(), parts.files.tmp_dir.clone());
        let (attachment, halt) = (attachment.clone(), halt.clone());
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            upload::prepare(&blob_dir, &tmp_dir, &attachment, captioned, &halt)
        })
    };
    let prepared = stop
        .until_stopped(async {
            job.await
                .unwrap_or_else(|_| Err(BackendError::internal("the file could not be prepared")))
        })
        .await?;
    // THE HOLD AGAIN, before the CDN (fix F3): the prepare can take long, and a lock another frame earned meanwhile
    // keeps the account from asking `w:m` and streaming the file for a message it may not send now. The prepared
    // file goes with this return.
    if let Some(held) = held_back(what, parts.hold.gate()) {
        return Err(held);
    }
    let uploaded = upload::send_file(session, &prepared, &halt, &reports).await?;
    let pointer = prepared.pointer(&uploaded, captioned);
    // The temporary file goes now (the upload answered; its readers are gone with the upload's future).
    drop(prepared);
    record::commit(
        &parts.store,
        conv.clone(),
        local.to_owned(),
        pointer.clone(),
        parts.now_ms,
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "an upload could not be recorded");
        BackendError::transient("the upload could not be recorded")
    })?;
    Ok(pointer)
}

/// The answer of a send the reach-out lock holds back (D8): `Transient{HOLD_RETRY}`, nothing written; `None` when the
/// gate is open.
fn waits_for_the_lock(what: &'static str, gate: Gate) -> Option<TransmitOutcome> {
    held_back(what, gate).map(TransmitOutcome::Failed)
}

/// The error of a send the reach-out lock holds back (D8), for the places that answer with a [`BackendError`].
pub(super) fn held_back(what: &'static str, gate: Gate) -> Option<BackendError> {
    if gate == Gate::Open {
        return None;
    }
    tracing::info!(
        kind = what,
        lock = if gate == Gate::Held {
            "held"
        } else {
            "unknown"
        },
        "a send waits for the reach-out lock"
    );
    Some(BackendError::new(
        ErrorKind::Transient {
            retry_after: Some(HOLD_RETRY),
        },
        "the account may not send yet",
    ))
}

/// What [`write_once`] gives when a frame of the local is in the air.
pub(super) enum Written {
    /// The wait ended within [`ACK_WAIT`]; the second field is the time left of a remembered nack's floor.
    Waited(Waited, Option<Duration>),
    /// Nothing within [`ACK_WAIT`]: the wait, still listening (step 7).
    TimedOut(Wait),
}

/// Step 7: what an attempt does after its transient reply when the ack did not come in time.
struct Late {
    wait: Wait,
    what: &'static str,
    session: Arc<dyn Session>,
    acks: Arc<Acks>,
    store: KourierStore,
    now_ms: i64,
    conv: ConvKey,
    local: String,
    kind: Kind,
    target: Option<String>,
}

impl Late {
    /// Ends the wait's connection if it is still the live one and no other wait is ending it (D5: the resend can
    /// go out only on a later connection), then records an ack or a 463 that still came for it (the late ack, so
    /// the core's resend is answered from the ledger). Returns when the wait ended.
    async fn listen(mut self) {
        end_unanswered(&self.wait, &self.acks, &*self.session, self.what).await;
        let Waited::Answered(answer) = self.wait.hear().await else {
            return;
        };
        let Some(outcome) = answer.recorded() else {
            // A nack the ledger does not keep: the server does not have the stanza, the resend may go.
            return;
        };
        let entry = Entry {
            conv: self.conv,
            local: self.local,
            kind: self.kind,
            target: self.target,
            outcome,
        };
        match ledger::commit(&self.store, entry, self.now_ms).await {
            Ok(_) => tracing::info!(kind = self.what, "a late ack was recorded"),
            Err(e) => {
                tracing::error!(kind = self.what, error = %e, "a late ack could not be recorded");
            }
        }
    }
}

/// Step 7's first half, shared with the timer set (`send::timer`): the connection of a frame nobody answered is ended,
/// once (the first wait of the connection to claim it), so the same id can go out only on a later connection.
pub(super) async fn end_unanswered(
    wait: &Wait,
    acks: &Acks,
    session: &dyn Session,
    what: &'static str,
) {
    let connection = wait.connection();
    if acks.claim_end(connection) {
        tracing::warn!(
            kind = what,
            "a frame heard no ack in time; the connection is ended"
        );
        session.drop_connection().await;
        acks.end(connection);
    }
}

/// The kind word of a request's frame in the two count lines (T2 F1): fixed words, never an id.
pub(super) fn frame_kind(kind: &OutgoingKind) -> &'static str {
    match kind {
        OutgoingKind::Message { .. } => "message",
        OutgoingKind::React { .. } => "reaction",
        OutgoingKind::Edit { .. } => "edit",
        OutgoingKind::Delete {
            for_everyone: true, ..
        } => "revoke",
        OutgoingKind::Delete { .. } => "other",
    }
}

/// Whether, and when, the core sends a written frame's local again (the third field of the second count line, T2 G1):
/// `no` when the answer is FINAL for the core (acked, refused, a permanent failure), else the seconds it waits at least.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Again {
    /// The core does not send this local again.
    No,
    /// The core sends it again after at least this long.
    After(Duration),
}

impl std::fmt::Display for Again {
    /// `no`, or the whole seconds rounded UP (a wait is never said shorter than it is).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::No => f.write_str("no"),
            Self::After(wait) => {
                let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                write!(f, "{secs}")
            }
        }
    }
}

/// The ONE reading of an error for the count line: the core's own rule (`ErrorKind::is_retryable`, the
/// `retry_after` its outbox waits at least) applied to the error the attempt answers the core with. There is no table
/// of codes here; [`nack_error`] (`send/errors.rs`) is the only one.
fn again_of(kind: &ErrorKind) -> Again {
    if kind.is_retryable() {
        Again::After(kind.retry_after().unwrap_or_default())
    } else {
        Again::No
    }
}

/// The word, the number and the `again` of the second count line for one end of a frame's wait (T2 F1, G1): `acked`
/// (code 0), `refused` (a 463 or a 475, with the code), `nacked` (any other nack, with its code, 0 when the code was no
/// number), `no-ack` (nothing within [`ACK_WAIT`]) and `connection-ended` (no answer will come on the connection).
/// `again` is read off the SAME error the attempt gives the core ([`nack_error`] with the floor the attempt uses,
/// [`no_ack`], [`connection_ended`]). A `timer` frame is answered to a command, not to the outbox: the core never sends
/// it again (the person asks again), whatever the error says.
fn answered_as(written: &Written, frame: &str) -> (&'static str, u16, Again) {
    let (outcome, code, kind) = match written {
        Written::TimedOut(_) => ("no-ack", 0, Some(no_ack().kind)),
        Written::Waited(Waited::Ended, _) => ("connection-ended", 0, Some(connection_ended().kind)),
        Written::Waited(Waited::Answered(Answer::Acked { .. }), _) => ("acked", 0, None),
        Written::Waited(Waited::Answered(Answer::Nacked(nack)), left) => {
            let code = match nack {
                Nack::Code(code) => *code,
                Nack::Unreadable => 0,
            };
            let outcome = if matches!(nack, Nack::Code(463 | 475)) {
                "refused"
            } else {
                "nacked"
            };
            // A remembered nack says what is left of its floor, a live one the whole floor (as the attempt does).
            let kind = nack_error(*nack, left.or_else(|| nack.floor())).kind;
            (outcome, code, Some(kind))
        }
    };
    let again = match kind {
        Some(_) if frame == "timer" => Again::No,
        Some(kind) => again_of(&kind),
        None => Again::No,
    };
    (outcome, code, again)
}

/// Registers the wait, writes the frame if it is the first of its local on the live connection, and waits for the
/// answer (at most [`ACK_WAIT`]). `Err` is the failure when no frame is in the air. `frame` is the kind word of the two
/// count lines (module docs): the first line follows a successful write (or a socket that failed while writing), the
/// second the end of the wait of a frame THIS call wrote.
pub(super) async fn write_once(
    session: &dyn Session,
    acks: &Acks,
    mut out: Outgoing,
    frame: &'static str,
) -> Result<Written, BackendError> {
    let mut refreshed = false;
    loop {
        let mut wait = acks.wait(&out.id);
        let mut wrote = false;
        if wait.first() {
            // The first frame after a 421's floor asks for fresh group metadata (fix F1).
            out.groups = if wait.refresh_groups() {
                Groups::Refresh
            } else {
                Groups::Cached
            };
            match session.send(out.clone()).await {
                // It may have left (a socket that failed while writing): the wait stays, and ends with the connection
                // this failure ends.
                Ok(()) | Err(SendFailure::Socket) => {
                    tracing::info!(kind = %frame, "a send frame was written");
                    wrote = true;
                }
                Err(failure) => {
                    acks.withdraw(&out.id, wait.connection());
                    if refresh_first(failure) && !refreshed {
                        refreshed = true;
                        out.devices = Devices::Refresh;
                        continue;
                    }
                    return Err(error_of(Cause::Failure(failure)));
                }
            }
        }
        let left = wait.left();
        let heard = tokio::time::timeout(ACK_WAIT, wait.hear()).await;
        let written = match heard {
            Ok(waited) => Written::Waited(waited, left),
            Err(_) => Written::TimedOut(wait),
        };
        if wrote {
            let (outcome, code, again) = answered_as(&written, frame);
            tracing::info!(outcome = %outcome, code, again = %again, "a send frame was answered");
        }
        return Ok(written);
    }
}

fn sent(server_t: Option<i64>) -> TransmitOutcome {
    TransmitOutcome::Sent {
        server_at: server_t.map(|t| t.saturating_mul(1000)),
        per_recipient: Vec::new(),
    }
}

pub(super) fn refused() -> BackendError {
    error_of(Cause::Nack(super::acks::Nack::Code(463)))
}

/// The answer to a request that makes no message (no frame).
fn unbuildable(what: &'static str, reason: Unbuildable) -> TransmitOutcome {
    match reason {
        Unbuildable::ForMe => {
            tracing::error!(
                kind = what,
                "FAULT: a delete for me reached the WhatsApp backend; nothing was sent"
            );
            sent(None)
        }
        Unbuildable::Address(Unaddressable::NoOwnId) => TransmitOutcome::Failed(
            BackendError::transient("the account's own ids are not known yet"),
        ),
        // A KEY that would name a person by number in a group that hides numbers (fix F20): never written. In a LID group
        // the mapping lacks the author's LID: for good (`Unsupported`, no retry), until it learns it.
        Unbuildable::Address(Unaddressable::NumberInLidGroup) => {
            tracing::info!(
                kind = what,
                reason = "number-in-lid-group",
                "a send that must name a person by number in a group that hides numbers was refused"
            );
            TransmitOutcome::Failed(BackendError::new(
                ErrorKind::Unsupported,
                "this would name a member by phone number in a group that hides numbers",
            ))
        }
        // The same in a group whose mode could not be read: the mode is asked again at the next send (fix F20).
        Unbuildable::Address(Unaddressable::GroupModeUnknown) => {
            tracing::info!(
                kind = what,
                reason = "group-mode-unknown",
                "a send that must name a person by number waits for the group's addressing mode"
            );
            TransmitOutcome::Failed(BackendError::transient(
                "the group's addressing mode is not known yet",
            ))
        }
        other => {
            tracing::info!(kind = what, reason = ?other, "a send makes no message");
            TransmitOutcome::Failed(BackendError::new(
                ErrorKind::Unsupported,
                format!("WhatsApp cannot send this ({other:?})"),
            ))
        }
    }
}

/// The ledger's kind of a request and the original it is about.
/// The edit slack rule (module docs): `original_t` the original's server time (s) when known, `sent_at_ms` when
/// the edit was made, `now_ms` now.
fn edit_too_late(original_t: Option<i64>, sent_at_ms: i64, now_ms: i64) -> bool {
    match original_t {
        Some(original_t) => now_ms.div_euclid(1000) - original_t > EDIT_SEND_LIMIT_SECS,
        // THE PROXY (named so in architecture §13): the original's age is unknown.
        None => {
            now_ms.saturating_sub(sent_at_ms)
                > i64::try_from(EDIT_WAIT_PROXY.as_millis()).unwrap_or(i64::MAX)
        }
    }
}

fn ledger_kind(kind: &OutgoingKind) -> (Kind, Option<String>) {
    match kind {
        OutgoingKind::Message { .. } => (Kind::Message, None),
        OutgoingKind::Edit { target, .. } => (Kind::Edit, Some(target.local.to_string())),
        OutgoingKind::Delete { target, .. } => (Kind::Revoke, Some(target.local.to_string())),
        OutgoingKind::React { .. } => (Kind::React, None),
    }
}

/// Every person a request names (the chat's peer, mentions, the authors of quoted and target messages): the
/// pairs the store is asked for.
fn users_of(req: &OutgoingRequest) -> Vec<UserId> {
    fn text(text: &Text, users: &mut Vec<UserId>) {
        for range in &text.ranges {
            if let RangeKind::Mention(user) = &range.kind {
                users.push(user.clone());
            }
        }
    }
    fn body(body: &Body, users: &mut Vec<UserId>) {
        match body {
            Body::Text(t)
            | Body::Media {
                caption: Some(t), ..
            } => text(t, users),
            _ => {}
        }
    }
    fn author(author: &TargetAuthor, users: &mut Vec<UserId>) {
        if let TargetAuthor::User(user) = author {
            users.push(user.clone());
        }
    }
    let mut users = Vec::new();
    if let ConvKey::Direct(user) = &req.conv {
        users.push(user.clone());
    }
    match &req.kind {
        OutgoingKind::Message {
            body: b, reply_to, ..
        } => {
            body(b, &mut users);
            if let Some(reply) = reply_to {
                author(&reply.target.author, &mut users);
                users.extend(reply.author.iter().cloned());
            }
        }
        OutgoingKind::Edit { target, body: b } => {
            author(&target.author, &mut users);
            body(b, &mut users);
        }
        OutgoingKind::Delete { target, .. } | OutgoingKind::React { target, .. } => {
            author(&target.author, &mut users);
        }
    }
    users
}

#[cfg(test)]
mod tests;
