//! The fake's upload phase of a message with files ([`Script::uploads`](crate::Script::uploads)):
//! what a backend that reports `AttachmentProgress` does before the server takes the message,
//! and how it answers `Command::AbortTransmit` meanwhile (the contract text on both).
//!
//! The phase runs as a task beside the command loop, so the actor keeps taking commands (the
//! abort among them). Its state is a [`Hold`]: the abort and the end of the phase are plain
//! functions on one thread with no `.await` between the look and the change, so exactly one of
//! them wins. A phase that was stopped sends nothing: the actor answers the `Transmit` `Failed`
//! without touching the fake server.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AbortAnswer, BackendEvent, Clock, Envelope, OutgoingKind, OutgoingRequest, TransmitOutcome,
};
use kourier_model::{BackendError, Body, ConvKey, LocalMsgId, SendPhase};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::script::UploadScript;

/// How often a moving upload reports (on the account clock): the contract's 250 ms.
const TICK: Duration = Duration::from_millis(250);
/// How often a held upload says its fixed shares again.
const SAY_AGAIN: Duration = Duration::from_secs(1);
/// The share of the message that is never reached before the phase is over.
const NOT_BEFORE_THE_END: f64 = 0.99;

/// Where the phase of one message stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Running: an abort stops it.
    Open,
    /// An abort was accepted (`Stopping` answered): the phase ends stopped.
    Requested,
    /// The phase ended: an abort comes too late.
    Closed,
}

/// The stop state of one message in its upload phase.
pub(crate) struct Hold {
    state: Cell<State>,
    wake: Notify,
    /// The script answers every abort `TooLate` and ends its phase at once.
    too_late: bool,
}

impl Hold {
    pub(crate) fn new(script: &UploadScript) -> Rc<Self> {
        Rc::new(Self {
            state: Cell::new(State::Open),
            wake: Notify::new(),
            too_late: matches!(script, UploadScript::TooLate { .. }),
        })
    }

    /// The answer to `Command::AbortTransmit`, decided at once.
    pub(crate) fn abort(&self) -> AbortAnswer {
        if self.too_late {
            // It comes too late whenever it comes; the held phase ends and the send goes out.
            self.wake.notify_one();
            return AbortAnswer::TooLate;
        }
        if self.state.get() == State::Open {
            self.state.set(State::Requested);
            self.wake.notify_one();
            AbortAnswer::Stopping
        } else {
            AbortAnswer::TooLate
        }
    }

    /// The phase is over; `false`: an abort won, the phase ends stopped.
    fn close(&self) -> bool {
        if self.state.get() == State::Requested {
            return false;
        }
        self.state.set(State::Closed);
        true
    }

    fn stopped(&self) -> bool {
        self.state.get() == State::Requested
    }

    /// Waits `d` on the account clock; `true` when the phase should end now (an abort was
    /// accepted, or the script is `TooLate` and one came).
    async fn rest(&self, clock: &dyn Clock, d: Duration) -> bool {
        tokio::select! {
            () = clock.sleep(d) => false,
            () = self.wake.notified() => true,
        }
    }
}

/// How a phase ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// Over: the fake server takes the message.
    Over,
    /// Stopped by an accepted abort: nothing goes to the server.
    Stopped,
}

/// What a finished phase hands back to the actor: the message, its request and the reply that
/// still waits for the outcome.
pub(crate) type Finished = (
    LocalMsgId,
    Box<OutgoingRequest>,
    oneshot::Sender<TransmitOutcome>,
    Ended,
);

/// The files of `req` that upload (position in the request, plaintext size) and the script for
/// its conversation, when the message has an upload phase: a message with files in a
/// conversation whose script has one. The rest of a long text is not a file for the user and
/// is never reported.
pub(crate) fn plan(
    uploads: &[(ConvKey, UploadScript)],
    req: &OutgoingRequest,
) -> Option<(Vec<(u16, u64)>, UploadScript)> {
    let OutgoingKind::Message {
        body: Body::Media { attachments, .. },
        ..
    } = &req.kind
    else {
        return None;
    };
    let script = uploads.iter().find(|(c, _)| *c == req.conv)?.1.clone();
    let files: Vec<(u16, u64)> = attachments
        .iter()
        .enumerate()
        .filter(|(_, a)| !a.long_text)
        .filter_map(|(i, a)| Some((u16::try_from(i).ok()?, a.size)))
        .collect();
    (!files.is_empty()).then_some((files, script))
}

/// What a stopped message answers: it failed before anything was sent to anyone.
pub(crate) fn stopped() -> BackendError {
    BackendError::internal("fake backend: stopped by the user before anything was sent")
}

/// The bytes a file is at when its share is `share` of `size` (rounded; a file is full only at
/// a share of 1: its upload is over then).
fn bytes_at(share: f32, size: u64) -> u64 {
    if share >= 1.0 {
        return size;
    }
    let done = (f64::from(share.max(0.0)) * size as f64).round();
    (done as u64).min(size.saturating_sub(1))
}

/// Says every file's progress (`done` bytes, parallel to `files`) and the message's share,
/// weighted by the sizes and never 1.0 before the phase is over.
async fn report(
    events: &mpsc::Sender<Envelope>,
    local: LocalMsgId,
    files: &[(u16, u64)],
    done: &[u64],
) {
    for ((index, total), done) in files.iter().zip(done) {
        let _ = events
            .send(Envelope::ephemeral(BackendEvent::AttachmentProgress {
                local,
                index: *index,
                done: *done,
                total: *total,
            }))
            .await;
    }
    let total: u64 = files.iter().map(|(_, t)| *t).sum();
    let have: u64 = done.iter().sum();
    let progress = if total == 0 {
        0.0
    } else {
        (have as f64 / total as f64).min(NOT_BEFORE_THE_END) as f32
    };
    let _ = events
        .send(Envelope::ephemeral(BackendEvent::TransmitProgress {
            local,
            progress,
            phase: Some(SendPhase::Uploading),
        }))
        .await;
}

/// The phase of one message. Owns everything it needs, so it runs beside the command loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    events: mpsc::Sender<Envelope>,
    clock: Arc<dyn Clock>,
    local: LocalMsgId,
    files: Vec<(u16, u64)>,
    script: UploadScript,
    hold: Rc<Hold>,
    req: Box<OutgoingRequest>,
    reply: oneshot::Sender<TransmitOutcome>,
) -> Finished {
    let ended = phase(&events, clock.as_ref(), local, &files, &script, &hold).await;
    (local, req, reply, ended)
}

async fn phase(
    events: &mpsc::Sender<Envelope>,
    clock: &dyn Clock,
    local: LocalMsgId,
    files: &[(u16, u64)],
    script: &UploadScript,
    hold: &Hold,
) -> Ended {
    let mut done = vec![0u64; files.len()];
    match script {
        UploadScript::Moving { step } => {
            report(events, local, files, &done).await;
            let step = f64::from(step.clamp(0.001, 1.0));
            for i in 0..files.len() {
                while done[i] < files[i].1 {
                    if hold.rest(clock, TICK).await && hold.stopped() {
                        return Ended::Stopped;
                    }
                    let add = ((files[i].1 as f64 * step).ceil() as u64).max(1);
                    done[i] = (done[i] + add).min(files[i].1);
                    report(events, local, files, &done).await;
                }
            }
        }
        UploadScript::Preparing { hold: lasts } => {
            let end = clock
                .now_ms()
                .saturating_add(u64::try_from(lasts.as_millis()).unwrap_or(u64::MAX));
            loop {
                let _ = events
                    .send(Envelope::ephemeral(BackendEvent::TransmitProgress {
                        local,
                        progress: 0.0,
                        phase: Some(SendPhase::Preparing),
                    }))
                    .await;
                let left = end.saturating_sub(clock.now_ms());
                if left == 0 {
                    break;
                }
                let wait = Duration::from_millis(left).min(SAY_AGAIN);
                if hold.rest(clock, wait).await {
                    // Only an accepted abort wakes it (the script is not `TooLate`).
                    return Ended::Stopped;
                }
            }
            for (i, (_, size)) in files.iter().enumerate() {
                done[i] = *size;
            }
            report(events, local, files, &done).await;
        }
        UploadScript::Held {
            shares,
            hold: lasts,
        }
        | UploadScript::TooLate {
            shares,
            hold: lasts,
        } => {
            for (i, (_, size)) in files.iter().enumerate() {
                done[i] = bytes_at(shares.get(i).copied().unwrap_or(0.0), *size);
            }
            let end = clock
                .now_ms()
                .saturating_add(u64::try_from(lasts.as_millis()).unwrap_or(u64::MAX));
            loop {
                report(events, local, files, &done).await;
                let left = end.saturating_sub(clock.now_ms());
                if left == 0 {
                    break;
                }
                let wait = Duration::from_millis(left).min(SAY_AGAIN);
                if hold.rest(clock, wait).await {
                    if hold.stopped() {
                        return Ended::Stopped;
                    }
                    // `TooLate`: the abort ended the hold.
                    break;
                }
            }
            for (i, (_, size)) in files.iter().enumerate() {
                done[i] = *size;
            }
            report(events, local, files, &done).await;
        }
    }
    if !hold.close() {
        return Ended::Stopped;
    }
    let _ = events
        .send(Envelope::ephemeral(BackendEvent::TransmitProgress {
            local,
            progress: 1.0,
            phase: None,
        }))
        .await;
    Ended::Over
}
