//! Scripts: what a [`FakeNetwork`](crate::FakeNetwork) account does after it starts.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_model::{
    BackendError, ConnState, ConvKey, HistoryTransfer, HistoryTransferFailure, IdError, UserId,
};

/// Why a script could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    /// A scripted id is malformed.
    #[error("invalid scripted id: {0}")]
    Id(#[from] IdError),
}

/// How the scripted login behaves after `StartLogin`.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginScript {
    /// Linked at once (fixture mode, crash tests).
    Auto,
    /// The Signal-like QR flow (§4.4): optional device-name input, `codes` rotating QR codes
    /// valid for `valid_for` each (on the account clock); the phone scans code number
    /// `scanned_at` (0-based), or none.
    Qr {
        /// Ask for a device name first.
        ask_device_name: bool,
        /// How many codes are shown before giving up.
        codes: u32,
        /// Validity (rotation interval) of each code.
        valid_for: Duration,
        /// The code the phone scans; `None` = never (the login fails with an expiry).
        scanned_at: Option<u32>,
        /// The identity the phone links (`None` = the script's own ids); a relink to another
        /// identity is refused.
        linked_as: Option<Vec<UserId>>,
    },
    /// The login fails right away.
    Fail {
        /// Log-safe detail.
        detail: String,
    },
}

/// One server delivery: stage 1 stores it in the inbox, stage 2 turns it into one
/// `core_outbox` row (§5.6).
#[derive(Clone, Debug, PartialEq)]
pub struct Delivery {
    /// The durable events.
    pub events: Vec<BackendEvent>,
    /// The final state depends on the position of this delivery (cross-conversation
    /// duplicates, roaming patches that overwrite each other): permutation tests keep it out.
    pub order_sensitive: bool,
}

impl Delivery {
    /// An order-insensitive delivery.
    pub fn new(events: Vec<BackendEvent>) -> Self {
        Self {
            events,
            order_sensitive: false,
        }
    }

    /// An order-sensitive delivery.
    pub fn ordered(events: Vec<BackendEvent>) -> Self {
        Self {
            events,
            order_sensitive: true,
        }
    }
}

/// One step of a script. Steps run in order once the account is linked and connected; the
/// cursor is durable in `backend.db`, so a restarted actor continues where it stopped.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// A server delivery of durable events.
    Deliver(Delivery),
    /// An ephemeral event (typing, progress), sent without an ack (boxed: it dwarfs the other
    /// steps).
    Ephemeral(Box<BackendEvent>),
    /// Publish a connection state.
    Conn(ConnState),
    /// The catch-up boundary (§4.5): wait until every delivery so far is acked by the core,
    /// then go `Online`.
    CatchUpDone,
    /// Wait until every delivery so far is acked.
    WaitAcked,
    /// Sleep on the account clock.
    Sleep(Duration),
    /// Panic the actor on the first `times` executions of this step (the count is durable).
    Panic {
        /// How many times.
        times: u32,
    },
    /// Exit with `Exit::Failed` (once).
    Fail(BackendError),
    /// The device is unlinked: drain the outbox and exit with `Exit::LoggedOut`. As the last
    /// step of a script, the script counts as finished once everything before it was acked.
    Unlink {
        /// Log-safe reason.
        reason: String,
    },
    /// The account's history transfer stands at this state, as after a restart in the
    /// middle of one (or after one that ended and was not dismissed): it is stored, reported
    /// now and again at every start and every `Command::Connect`, a skip ends a wait or a
    /// download as skipped, and a dismissal removes an end. Nothing walks it on: a state
    /// that is not over stays until it is skipped.
    Transfer(HistoryTransfer),
}

/// The history transfer of a fresh link (architecture §14.2 item 14), as a backend runs it
/// between the link and the login's `Done` step: the states it walks and how it ends. While
/// it runs the account reports `Syncing`, its script's steps wait (nothing is delivered),
/// and the fake obeys the two commands as the contract says: `SkipHistoryTransfer` ends a
/// wait or a download as `Skipped` (the next event), and does nothing while the history is
/// read; `DismissHistoryTransfer` removes an end, so it is not reported again. The stage
/// reached is durable: a restarted actor reports it and goes on from there. A relink
/// ("Link Again") has no transfer.
#[derive(Clone, Debug, PartialEq)]
pub struct TransferScript {
    /// The states the transfer walks, in order: each is reported and held for its time on
    /// the account clock (`None`: until a skip, which only a wait or a download takes).
    pub stages: Vec<(HistoryTransfer, Option<Duration>)>,
    /// How it ends when every stage ran out: done, skipped (the phone's user declined, or
    /// the phone did not answer) or failed with a kind.
    pub end: HistoryTransfer,
    /// The messages of the history, counted while it is read as Signal's delivery counts
    /// them (`BackendEvent::AccountProgress`): a read held for a time reports 0, ¼, ½ and ¾
    /// of them over its time, one held until it is skipped reports [`Self::HELD_SHARE`] of
    /// them; the walk's end reports `AccountProgress::finished()`. `None`: no numbers.
    pub import: Option<u64>,
}

impl TransferScript {
    /// The size of the download of [`Self::walk`] and [`Self::failing`], in bytes.
    pub const TOTAL: u64 = 45_600_000;

    /// The messages of the history of [`Self::walk`] and [`Self::failing`].
    pub const MESSAGES: u64 = 50_000;

    /// The share of [`Self::import`], in thousandths, a read that is held reports.
    pub const HELD_SHARE: u64 = 248;

    /// A whole transfer: the wait, a download whose size becomes known after its first
    /// report, the read, and `end`. Every stage is held for `hold`.
    pub fn walk(hold: Duration, end: HistoryTransfer) -> Self {
        let total = Some(Self::TOTAL);
        let part = |done, total| HistoryTransfer::Downloading { done, total };
        Self {
            stages: vec![
                (HistoryTransfer::Waiting, Some(hold)),
                (part(0, None), Some(hold / 4)),
                (part(4_100_000, total), Some(hold / 4)),
                (part(12_300_000, total), Some(hold / 4)),
                (part(31_900_000, total), Some(hold / 4)),
                (part(Self::TOTAL, total), Some(hold / 4)),
                (HistoryTransfer::Reading, Some(hold)),
            ],
            end,
            import: Some(Self::MESSAGES),
        }
    }

    /// A transfer that reaches `state` and stays there until it is skipped (for good, when
    /// the state cannot be skipped): a wait, a download, the read.
    pub fn holding(state: HistoryTransfer) -> Self {
        Self {
            stages: vec![(state, None)],
            end: HistoryTransfer::Skipped,
            import: (state == HistoryTransfer::Reading).then_some(Self::MESSAGES),
        }
    }

    /// A short wait (`hold`), then `end`: the phone's user declined (`Skipped`), the phone
    /// asked for a new link (`Failed(LinkAgain)`), and so on.
    pub fn ending(hold: Duration, end: HistoryTransfer) -> Self {
        Self {
            stages: vec![(HistoryTransfer::Waiting, Some(hold))],
            end,
            import: None,
        }
    }

    /// A transfer that fails with `kind` at the point where a real one does: before the
    /// download (`LinkAgain`), in it (`Download`, `TooLarge`) or in the read (the others).
    pub fn failing(hold: Duration, kind: HistoryTransferFailure) -> Self {
        let mut walk = Self::walk(hold, HistoryTransfer::Failed(kind));
        let keep = match kind {
            HistoryTransferFailure::LinkAgain => 1,
            HistoryTransferFailure::Download | HistoryTransferFailure::TooLarge => 4,
            HistoryTransferFailure::UpdateApp
            | HistoryTransferFailure::Corrupt
            | HistoryTransferFailure::Internal
            | HistoryTransferFailure::Incomplete => walk.stages.len(),
        };
        walk.stages.truncate(keep);
        walk
    }
}

/// A scripted transmit failure (§10.2 "transmit failures by kind including `Partial`"). The
/// count is durable in `backend.db`, so a restarted actor continues it.
#[derive(Clone, Debug, PartialEq)]
pub struct TransmitRule {
    /// Only sends in this conversation (`None` = any).
    pub conv: Option<ConvKey>,
    /// `None`: the whole attempt fails with `Failed(error)`. `Some(member)`: only that
    /// recipient fails, the others are accepted (`Partial`).
    pub member: Option<UserId>,
    /// The error.
    pub error: BackendError,
    /// How many matching attempts fail.
    pub times: u32,
    /// Only requests of this kind (`OutgoingKind::name`: `Message`, `Edit`, `Delete`, `React`;
    /// `None` = any).
    pub kind: Option<String>,
}

impl TransmitRule {
    /// The first `times` attempts in any conversation fail with `error`.
    pub fn attempts(error: BackendError, times: u32) -> Self {
        Self {
            conv: None,
            member: None,
            error,
            times,
            kind: None,
        }
    }

    /// `member` fails with `error` in the first `times` attempts that include them.
    pub fn member(member: UserId, error: BackendError, times: u32) -> Self {
        Self {
            conv: None,
            member: Some(member),
            error,
            times,
            kind: None,
        }
    }

    /// Restricts the rule to one conversation.
    pub fn in_conv(mut self, conv: ConvKey) -> Self {
        self.conv = Some(conv);
        self
    }

    /// Restricts the rule to requests of one kind (`"Edit"`, `"Delete"`, `"React"`,
    /// `"Message"`).
    pub fn of_kind(mut self, kind: &str) -> Self {
        self.kind = Some(kind.to_owned());
        self
    }

    /// Every attempt fails with a challenge (`ErrorKind::Challenge` with `url`) until the
    /// user's answer was accepted (`Command::SubmitChallenge`, see
    /// [`Script::challenge_answer`]), like a server that answers 428 until its captcha is
    /// solved. Rules whose error is a challenge stop applying once one was solved.
    pub fn challenge(url: &str) -> Self {
        Self::attempts(challenge_error(url), u32::MAX)
    }

    /// Like [`challenge`](Self::challenge), but only `member` is refused: the others are
    /// accepted (`Partial`).
    pub fn member_challenge(member: UserId, url: &str) -> Self {
        Self::member(member, challenge_error(url), u32::MAX)
    }
}

/// The error of a scripted challenge: `ErrorKind::Challenge` with `url` and a fake token, and
/// no retry time (the server waits for the answer).
pub fn challenge_error(url: &str) -> BackendError {
    challenge(url, None)
}

/// A scripted challenge the server lifts by itself: `ErrorKind::Challenge` with `url`, the fake
/// token and `retry_after` (a 428 with a Retry-After). A rule that fails a counted number of
/// attempts with it models a server that takes sends again once that time has passed.
pub fn challenge_error_after(url: &str, retry_after: Duration) -> BackendError {
    challenge(url, Some(retry_after))
}

fn challenge(url: &str, retry_after: Option<Duration>) -> BackendError {
    BackendError::new(
        kourier_model::ErrorKind::Challenge {
            token: kourier_model::Redacted::new(CHALLENGE_TOKEN.into()),
            url: url.into(),
            retry_after,
        },
        "challenge required",
    )
}

/// The token of every scripted challenge; the fake refuses an answer that comes with another
/// one (the core must hand the token of the paused attempt back).
pub const CHALLENGE_TOKEN: &str = "fake-challenge-token";

/// A complete scripted account.
#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    /// Name for logs and test output.
    pub name: String,
    /// Our ids, reported in `AccountReady`.
    pub own_ids: Vec<UserId>,
    /// Our device id.
    pub device: Option<u32>,
    /// Our profile name.
    pub display_name: Option<String>,
    /// Our number, reported in `AccountReady` when set.
    pub number: Option<String>,
    /// The login.
    pub login: LoginScript,
    /// What happens after login.
    pub steps: Vec<Step>,
    /// Refuse roaming changes like a rejected Threema transaction (§7.3).
    pub reject_changes: bool,
    /// Delay before each forwarded batch, on the account clock ("slow acks").
    pub forward_delay: Option<Duration>,
    /// Scripted transmit failures, checked in order; anything else is accepted for every
    /// recipient.
    pub transmit: Vec<TransmitRule>,
    /// Breaks the contract on purpose: durable events committed during the link *before*
    /// `AccountReady` (the core must refuse them, §4.3).
    pub early_events: Vec<BackendEvent>,
    /// A restarted actor of a linked account reports `Syncing` until the core acked what it
    /// had in `core_outbox`, plus this long on the account clock, before `Online` (a real
    /// backend's catch-up boundary, §4.5). `None`: `Online` at once.
    pub catch_up_on_resume: Option<Duration>,
    /// Late transmit answers: a request in one of these conversations is taken by the fake
    /// server at once (its outcome is decided and recorded), but the reply reaches the core only
    /// after the delay on the account clock. The actor handles other commands meanwhile.
    pub reply_delays: Vec<(ConvKey, Duration)>,
    /// Slow sends in EVERY conversation (fixture runs that show the line under a message on its
    /// way, link feedback L6): each transmit reports `phase` (as `BackendEvent::TransmitProgress`)
    /// and its answer reaches the core `after` this long on the account clock. Rules in
    /// [`Self::reply_delays`] win for their conversations. `None`: answers at once.
    pub slow_sends: Option<(Option<kourier_model::SendPhase>, Duration)>,
    /// The answer the fake server accepts for a challenge (`Command::SubmitChallenge`); `None`
    /// accepts any answer. A wrong answer is refused and the challenge stands.
    pub challenge_answer: Option<String>,
    /// While linking, report `Online` before `AccountReady` is committed: a backend whose
    /// socket is up before the core has accepted (or, on a relink, refused) its identity. The
    /// core must not send anything through it until then.
    pub online_while_linking: bool,
    /// Durable events committed right behind `AccountReady` (rows of their own, before the
    /// forwarder runs), so they ship in one batch with it: a backend that emits its first
    /// state at once. A core that refuses the batch gets its rows again one by one.
    pub with_ready: Vec<BackendEvent>,
    /// The first this many relinks of an account that runs this script fail (a login step
    /// `Failed`, as when the phone refuses the link) before one succeeds. Counted per account
    /// while the process lives.
    pub fail_relinks: u32,
    /// Every download takes this long on the account clock before its file is written and
    /// its answer committed (a slow network). The actor takes no other command meanwhile;
    /// its script and its forwarder go on.
    pub download_delay: Option<Duration>,
    /// The history transfer a fresh link of this account starts ([`TransferScript`]); `None`:
    /// the link has none, like a network without history.
    pub transfer: Option<TransferScript>,
    /// How the account's phone answers a request to upload lost files again
    /// (`Command::RequestAttachments`, [`BackfillScript`]); `None`: the account cannot ask
    /// (no `attachment_restore` capability), so a lost file stays lost.
    pub backfill: Option<BackfillScript>,
    /// The fake server cannot be reached for a logout (`Command::Logout`): the report says
    /// `NotReached` and the server keeps the device, as when the network is down while the
    /// user removes or unlinks the account (link feedback L4).
    pub logout_unreachable: bool,
    /// The name the device was linked with, reported with the logout (`LogoutReport`).
    pub device_name: Option<String>,
    /// The fake takes this long (on its clock) to answer a logout: a test sees the account
    /// `Unlinking` or `Removing` meanwhile.
    pub logout_delay: Option<std::time::Duration>,
    /// How many progress events the fake sends before its report of a logout, and then exits
    /// at once (without draining its outbox), so that the actor's exit is ready while the core
    /// has not read the report yet: a test of the core taking the report after the exit. At
    /// most 6 (the event queue holds 8).
    pub logout_backlog: usize,
}

/// How the fake account's phone answers a request for the files of a message the network
/// lost (S6): the request is reported `Sent` first, then this.
#[derive(Clone, Debug, PartialEq)]
pub enum BackfillScript {
    /// It never answers: the core's wait ends as "did not answer".
    Silent,
    /// It no longer has the message.
    NotFound,
    /// One answer, a slot per file in the message's order: `Some` uploads it again (the
    /// download yields that pointer's bytes), `None` it does not have that file.
    Files(Vec<Option<crate::attachments::Pointer>>),
}

impl Script {
    /// A script with automatic login and no steps.
    pub fn new(name: impl Into<String>, own_ids: Vec<UserId>) -> Self {
        Self {
            name: name.into(),
            own_ids,
            device: Some(2),
            display_name: None,
            number: None,
            login: LoginScript::Auto,
            steps: Vec::new(),
            reject_changes: false,
            forward_delay: None,
            transmit: Vec::new(),
            early_events: Vec::new(),
            catch_up_on_resume: None,
            reply_delays: Vec::new(),
            slow_sends: None,
            challenge_answer: None,
            online_while_linking: false,
            with_ready: Vec::new(),
            fail_relinks: 0,
            download_delay: None,
            transfer: None,
            backfill: None,
            logout_unreachable: false,
            logout_backlog: 0,
            logout_delay: None,
            device_name: None,
        }
    }

    /// Lets the account ask its phone for lost files, answered as `backfill` says
    /// ([`backfill`](Self::backfill)): every `AccountCaps` the script delivers gets the
    /// `attachment_restore` flag, and one with only that flag is delivered first when it
    /// has none. Call it after the deliveries that carry the account's caps.
    pub fn with_backfill(mut self, backfill: BackfillScript) -> Self {
        use kourier_model::AccountCaps;
        self.backfill = Some(backfill);
        let flag = AccountCaps::FLAG_ATTACHMENT_RESTORE.to_owned();
        let mut found = false;
        for step in &mut self.steps {
            if let Step::Deliver(d) = step {
                for e in &mut d.events {
                    if let BackendEvent::AccountCaps(caps) = e {
                        caps.flags.insert(flag.clone(), true);
                        found = true;
                    }
                }
            }
        }
        if !found {
            let mut caps = AccountCaps::default();
            caps.flags.insert(flag, true);
            self.steps.insert(
                0,
                Step::Deliver(Delivery::new(vec![BackendEvent::AccountCaps(caps)])),
            );
        }
        self
    }

    /// Gives a fresh link of the account a history transfer ([`transfer`](Self::transfer)).
    pub fn with_transfer(mut self, transfer: TransferScript) -> Self {
        self.transfer = Some(transfer);
        self
    }

    /// Reports `number` as the account's own number ([`number`](Self::number)).
    pub fn with_number(mut self, number: &str) -> Self {
        self.number = Some(number.to_owned());
        self
    }

    /// Makes every download take `delay` ([`download_delay`](Self::download_delay)).
    pub fn slow_downloads(mut self, delay: Duration) -> Self {
        self.download_delay = Some(delay);
        self
    }

    /// Makes the first `times` relinks fail ([`fail_relinks`](Self::fail_relinks)).
    pub fn fail_relink(mut self, times: u32) -> Self {
        self.fail_relinks = times;
        self
    }

    /// Commits `events` right behind `AccountReady`, one row each
    /// ([`with_ready`](Self::with_ready)).
    pub fn ready_with(mut self, events: Vec<BackendEvent>) -> Self {
        self.with_ready.extend(events);
        self
    }

    /// Reports `Online` during the link, before `AccountReady`
    /// ([`online_while_linking`](Self::online_while_linking)).
    pub fn online_while_linking(mut self) -> Self {
        self.online_while_linking = true;
        self
    }

    /// Accepts only `answer` for a challenge.
    pub fn accept_challenge_answer(mut self, answer: &str) -> Self {
        self.challenge_answer = Some(answer.to_owned());
        self
    }

    /// Holds the answers to transmits in `conv` for `delay` on the account clock.
    pub fn delay_reply(mut self, conv: ConvKey, delay: Duration) -> Self {
        self.reply_delays.push((conv, delay));
        self
    }

    /// Every transmit reports `phase` and is answered `after` this long
    /// ([`Self::slow_sends`]).
    pub fn slow_sends(mut self, phase: Option<kourier_model::SendPhase>, after: Duration) -> Self {
        self.slow_sends = Some((phase, after));
        self
    }

    /// Adds a transmit failure rule.
    pub fn fail_transmit(mut self, rule: TransmitRule) -> Self {
        self.transmit.push(rule);
        self
    }

    /// Appends a delivery.
    pub fn deliver(mut self, events: Vec<BackendEvent>) -> Self {
        self.steps.push(Step::Deliver(Delivery::new(events)));
        self
    }

    /// Appends an order-sensitive delivery.
    pub fn deliver_ordered(mut self, events: Vec<BackendEvent>) -> Self {
        self.steps.push(Step::Deliver(Delivery::ordered(events)));
        self
    }

    /// Appends any step.
    pub fn step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    /// The event lists of every delivery, in order (what the core receives, one envelope per
    /// delivery when the forwarder does not batch).
    pub fn deliveries(&self) -> Vec<Vec<BackendEvent>> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Deliver(d) => Some(d.events.clone()),
                _ => None,
            })
            .collect()
    }

    /// The deliveries whose final effect does not depend on their position.
    pub fn permutable_deliveries(&self) -> Vec<Vec<BackendEvent>> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Deliver(d) if !d.order_sensitive => Some(d.events.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every envelope this script makes the backend send, without acks wired (for the
    /// conformance suite): one durable envelope per delivery, one ephemeral per ephemeral step.
    pub fn envelopes(&self) -> Vec<Envelope> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Deliver(d) => Some(Envelope::durable(d.events.clone()).0),
                Step::Ephemeral(e) => Some(Envelope::ephemeral(e.as_ref().clone())),
                _ => None,
            })
            .collect()
    }
}
