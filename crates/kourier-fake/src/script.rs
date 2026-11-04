//! Scripts: what a [`FakeNetwork`](crate::FakeNetwork) account does after it starts.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_model::{
    BackendError, CodeDelivery, ConnState, ConvKey, HistoryTransfer, HistoryTransferFailure,
    IdError, UserId,
};

/// Why a script could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    /// A scripted id is malformed.
    #[error("invalid scripted id: {0}")]
    Id(#[from] IdError),
}

/// The three emoji the fake shows at the emoji step of a QR login it plays as Threema (K1,
/// TH4 KH1c): fixed, and three of the 128 of Threema's own list, so the viewmodel's names and the
/// pictures treat them as the real thing (the black cat is the list's emoji sequence with a
/// zero-width joiner).
pub const FAKE_EMOJI: [&str; 3] = ["\u{1F435}", "\u{1F408}\u{200D}\u{2B1B}", "\u{1F33B}"];

/// How long the emoji step of a QR login played as Threema stands before the link goes on, on
/// the account clock, unless the script says otherwise ([`Script::emoji_for`]): a person walking
/// the link by hand sees the three emoji for this long.
pub const EMOJI_FOR: Duration = Duration::from_secs(2);

/// How the scripted login behaves after `StartLogin`.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginScript {
    /// Linked at once (fixture mode, crash tests).
    Auto,
    /// The Signal-like QR flow (§4.4): optional device-name input, `codes` rotating QR codes
    /// valid for `valid_for` each (on the account clock); the phone scans code number
    /// `scanned_at` (0-based), or none. A fake that plays Threema shows the emoji step after the
    /// scan ([`FAKE_EMOJI`], [`Script::emoji_for`]), as the real backend does.
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

/// The pair-code flow of a network that has one (WhatsApp's "link with phone number", flow id
/// `pair-code`): the actor asks for the phone number (an `Input` step), may refuse it as the
/// network does (`refusals`: the n-th pair-code attempt of the network gets the n-th error, then
/// they are accepted), shows `code` (`ShowCode`), and the phone types it `entered_after` later
/// on the account clock. Without this script a `pair-code` login is refused as unsupported.
#[derive(Clone, Debug, PartialEq)]
pub struct PairCodeScript {
    /// The errors of the first attempts, in order (a rate limit, "not available").
    pub refusals: Vec<BackendError>,
    /// The code shown (the real one is 8 characters).
    pub code: String,
    /// How long the code is valid, on the account clock.
    pub valid_for: Duration,
    /// When the phone enters the code after it was shown; `None`: never (the login fails with
    /// an expiry once `valid_for` has passed).
    pub entered_after: Option<Duration>,
}

impl PairCodeScript {
    /// Shows `code` for three minutes; the phone enters it after ten seconds; no refusals.
    pub fn new(code: &str) -> Self {
        Self {
            refusals: Vec::new(),
            code: code.to_owned(),
            valid_for: Duration::from_secs(180),
            entered_after: Some(Duration::from_secs(10)),
        }
    }

    /// The first attempts are refused with `refusals`.
    pub fn refusing(mut self, refusals: Vec<BackendError>) -> Self {
        self.refusals = refusals;
        self
    }

    /// The phone never enters the code.
    pub fn never_entered(mut self) -> Self {
        self.entered_after = None;
        self
    }
}

/// The two-step verification of an account (Telegram's password): what is typed, and the hint the
/// account's owner gave, which the step shows as the field's note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswordScript {
    /// The right password.
    pub password: String,
    /// The owner's own hint; `None`: the account set none.
    pub hint: Option<String>,
}

/// One account of a network whose login asks for a phone number (Telegram's `phone` flow): what the
/// network does with that NUMBER. The table of [`PhoneLoginScript`] is keyed on the digits the person
/// types, never on the attempt, so a rehearsal that restarts the app between attempts meets the same
/// answers.
#[derive(Clone, Debug, PartialEq)]
pub struct PhoneAccount {
    /// The number's digits only (the key; no `+`, no separators).
    pub digits: String,
    /// The network refuses this number at `phone` with this error (a flood wait, a ban); `None`:
    /// it sends the code.
    pub refusal: Option<BackendError>,
    /// The right code.
    pub code: String,
    /// What the code step states: how the code was sent, and the wait before a new one may be asked
    /// for (`resend.after` is the network's whole wait on the account clock; the step states the part
    /// that is left).
    pub delivery: CodeDelivery,
    /// The account has two-step verification: asked after the code, and after a QR scan.
    pub password: Option<PasswordScript>,
    /// The network asks for a login e-mail address after the password: this is the right code it
    /// sends there. `None`: no e-mail step.
    pub email_code: Option<String>,
}

impl PhoneAccount {
    /// An account at `number` (its digits are kept; `+` and separators are dropped) that sends
    /// `code` the way `delivery` says; no refusal, no password, no e-mail.
    pub fn new(number: &str, code: &str, delivery: CodeDelivery) -> Self {
        Self {
            digits: digits_of(number),
            refusal: None,
            code: code.to_owned(),
            delivery,
            password: None,
            email_code: None,
        }
    }

    /// The network refuses this number at `phone` with `error`.
    pub fn refusing(mut self, error: BackendError) -> Self {
        self.refusal = Some(error);
        self
    }

    /// The account has two-step verification (`hint` is its owner's own words).
    pub fn with_password(mut self, password: &str, hint: Option<&str>) -> Self {
        self.password = Some(PasswordScript {
            password: password.to_owned(),
            hint: hint.map(str::to_owned),
        });
        self
    }

    /// The network asks for a login e-mail address, then for `email_code` sent there.
    pub fn with_email(mut self, email_code: &str) -> Self {
        self.email_code = Some(email_code.to_owned());
        self
    }
}

/// The digits of `typed` (every ASCII digit, in order).
pub(crate) fn digits_of(typed: &str) -> String {
    typed.chars().filter(char::is_ascii_digit).collect()
}

/// The `phone` login flow of a network that sends a login code (Telegram, KT-1b): the number is
/// asked for, and the ACCOUNT it names decides everything after it ([`PhoneAccount`]). A typed
/// number that is in no row fails at `registration` (the network has no account for it); one that
/// is a row follows the row: its refusal at `phone`, else the code step (with its delivery and its
/// resend), the password, the e-mail steps, the link. The tail the run tool expects
/// (`BackendConfig::expect_number_suffix`) is checked on the typed number BEFORE the code, when the
/// script honours it ([`Script::honour_number_tail`]). Without this script a `phone` login is
/// refused as unsupported.
#[derive(Clone, Debug, PartialEq)]
pub struct PhoneLoginScript {
    /// The accounts, one row per number.
    pub numbers: Vec<PhoneAccount>,
}

impl PhoneLoginScript {
    /// A table of `numbers`.
    pub fn new(numbers: Vec<PhoneAccount>) -> Self {
        Self { numbers }
    }

    /// The row of the number whose digits are `digits`.
    pub(crate) fn account(&self, digits: &str) -> Option<&PhoneAccount> {
        self.numbers.iter().find(|a| a.digits == digits)
    }
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

/// How many chats the fake lets an account pin (`AccountCaps::max_pinned_chats`): the common limit of 4. It is
/// the statement of a script that has [`Script::roaming_writes`] and states no limit of its own; a script
/// states another one in an `AccountCaps` of its own (see [`Script::roaming_writes`]).
pub const PIN_LIMIT: u32 = 4;

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
    /// The `pair-code` login flow, when the network has one ([`PairCodeScript`]).
    pub pair_code: Option<PairCodeScript>,
    /// The `phone` login flow, when the network has one ([`PhoneLoginScript`]); also what the
    /// account the phone scans in is asked after a QR scan: the password of the row of
    /// [`Self::number`] (TDLib's `WaitPassword` after a scan).
    pub phone_login: Option<PhoneLoginScript>,
    /// What happens after login.
    pub steps: Vec<Step>,
    /// What the network states at EVERY connect, `delay` (account clock) after the connection
    /// is `Online` ([`after_connect`](Self::after_connect)): a backend that restates its
    /// account hold only once it is up.
    pub after_connect: Vec<(Duration, BackendEvent)>,
    /// Refuse roaming changes like a rejected Threema transaction (§7.3).
    pub reject_changes: bool,
    /// What the fake states of the settings its network keeps for the account's other devices
    /// (archive, pin, mute, marked unread: "roaming"), in the `AccountCaps` it delivers
    /// ([`kourier_model::AccountCaps::roaming_writes`]): `Some(true)` = the network writes them
    /// and the account's switch is on (the DEFAULT, so every fake-driven test and fixture page that
    /// archives or pins keeps working), `Some(false)` = it can and the switch is off, `None` =
    /// the network cannot and the fake states nothing, like WhatsApp, Telegram and Threema until
    /// they write. When it is `Some`, the fake adds the statement and [`PIN_LIMIT`] to the first
    /// delivery that carries no `AccountCaps` of its own and to every `AccountCaps` of the script
    /// that does not state the flag itself, refuses a roaming change while the stated flag is
    /// not `Some(true)`, and obeys `Command::SetRoamingWrites`.
    ///
    /// The fake plays the couplings and the limit that such networks have in common (a new pin goes
    /// to the end of the pin list a network keeps and unarchives the chat, an archive unpins, at most
    /// [`PIN_LIMIT`] pins), whatever [`Flavor`](crate::Flavor) it plays. A script that plays another
    /// limit (a network with 3 pins) states it in an `AccountCaps` of its own that carries
    /// `max_pinned_chats: Some(3)`: the fake keeps the number a delivered `AccountCaps` states and adds
    /// [`PIN_LIMIT`] only to caps that state none. A script whose own caps state the flag and no limit
    /// plays a network without one. Test: `conformance::a_script_states_its_own_pin_limit`.
    pub roaming_writes: Option<bool>,
    /// Delay before each forwarded batch, on the account clock ("slow acks"). On
    /// [`Flavor::TelegramLike`](crate::Flavor::TelegramLike) it also decides the order of the
    /// two things the server says of an accepted message: the echo is committed with the accept,
    /// and by default the core has it (acked) before it hears the outcome; with a forward delay
    /// the outcome goes out at once and the echo ships behind it. A slow answer
    /// ([`reply_delays`](Self::reply_delays)) keeps the echo first.
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
    /// The echo of an accepted message states no `allocated_as`
    /// ([`Flavor::TelegramLike`](crate::Flavor::TelegramLike) only; the other flavours send no
    /// echo): an echo that knows no binding, which the core joins to the message when the
    /// outcome comes. Default off: the echo names the id the message was allocated under.
    pub echo_without_binding: bool,
    /// Slow sends in EVERY conversation (fixture runs that show the line under a message on its
    /// way, link feedback L6): each transmit reports `phase` (as `BackendEvent::TransmitProgress`)
    /// and its answer reaches the core `after` this long on the account clock. Rules in
    /// [`Self::reply_delays`] win for their conversations. `None`: answers at once.
    pub slow_sends: Option<(Option<kourier_model::SendPhase>, Duration)>,
    /// A message with files sent in one of these conversations has an UPLOAD PHASE before the
    /// fake server takes it ([`UploadScript`]): the actor reports each file's progress
    /// (`BackendEvent::AttachmentProgress`, `index` = the file's position in the request) and
    /// obeys `Command::AbortTransmit` while the phase lasts. Nothing reaches the fake server
    /// before the phase ends, so an aborted send leaves no `fake_transmits` row and no
    /// [`FakeControl::transmits`](crate::FakeControl::transmits) record. Edits, deletes,
    /// reactions and messages without files have none. By default the fake reports no upload.
    pub uploads: Vec<(ConvKey, UploadScript)>,
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
    /// Honour `BackendConfig::expect_number_suffix` at the pairing of the QR flow, like the real
    /// WhatsApp backend does (W2c): when a tail is set and [`Self::number`] does not end in it,
    /// the login fails at the `qr` step with `ErrorKind::NotTheExpectedAccount` and links
    /// nothing (no `AccountReady`, no device). Off by default: the other fakes ignore the tail,
    /// as they always did (the T1a rehearsal switches it on).
    pub honour_number_tail: bool,
    /// The fake server cannot be reached for a logout (`Command::Logout`): the report says
    /// `NotReached` and the server keeps the device, as when the network is down while the
    /// user removes or unlinks the account (link feedback L4).
    pub logout_unreachable: bool,
    /// The name the device was linked with, reported with the logout (`LogoutReport`).
    pub device_name: Option<String>,
    /// How long the emoji step of a QR login stands (account clock), for a fake that plays
    /// Threema ([`FAKE_EMOJI`] after the scan; another fake has no such step). Default
    /// [`EMOJI_FOR`]; a fixture page that must stand at the step gives it a long one.
    pub emoji_for: Duration,
    /// How long the FIRST code of the QR flow is valid, when that differs from the other codes'
    /// `valid_for` ([`LoginScript::Qr`]): the rehearsal of a run tool that must catch the first
    /// code on screen (a long one nobody scans), then link on a short second one. `None` (the
    /// default): every code lives `valid_for`.
    pub first_qr_for: Option<Duration>,
    /// The fake takes this long (on its clock) to answer a logout: a test sees the account
    /// `Unlinking` or `Removing` meanwhile.
    pub logout_delay: Option<std::time::Duration>,
    /// How many progress events the fake sends before its report of a logout, and then exits
    /// at once (without draining its outbox), so that the actor's exit is ready while the core
    /// has not read the report yet: a test of the core taking the report after the exit. At
    /// most 6 (the event queue holds 8).
    pub logout_backlog: usize,
    /// How many progress events the actor sends from its command loop before it answers an
    /// `AbortTransmit`: a backend whose loop is inside an emit when the X comes (the Signal
    /// actor's `Connect` with a stored transfer is one). The event queue holds 8, so with more
    /// the actor waits for the core to READ before it can answer: a core that waits for the
    /// answer without reading events would run into its time-out and take the stop for
    /// `TooLate` (upload progress, fix K3).
    pub abort_backlog: usize,
    /// The safety numbers of the network (P5a, critic F15): the people who have one and the
    /// states their key goes through ([`SafetyScript`]). Empty (the default): the fake answers
    /// `Command::SafetyNumber` with `SafetyUnavailable::Unsupported`, as a network without
    /// safety numbers, and refuses `SetVerified`. Sends that meet a changed key are scripted
    /// with [`TransmitRule`]s (`ErrorKind::UntrustedIdentity`), decided before the run.
    pub safety: Vec<SafetyScript>,
}

/// One person's safety numbers in the fake ([`Script::safety`]). The fake HOLDS one entry of
/// `keys` at a time, starting with the first: a `Command::SafetyNumber` answers the held entry
/// and then the fake moves on to the next (the last one stays), so a script says "the key
/// changed after the page asked" as two entries. `SetVerified` is refused (`InvalidInput`)
/// when its key is not the held one; otherwise it sets the held entry's state, states the
/// contact's trust and key (`ContactUpsert`), writes the quiet `VerifiedChanged` notice into
/// the 1:1 chat when the state changed, and tells `IdentityTrusted` when it was `Changed`.
/// The digits and the 74 scannable bytes are made from the key (synthetic, not Signal's).
#[derive(Clone, Debug, PartialEq)]
pub struct SafetyScript {
    /// The person.
    pub user: UserId,
    /// The keys the fake holds in turn, each with its starting state.
    pub keys: Vec<(
        kourier_model::IdentityFingerprint,
        kourier_model::VerifyState,
    )>,
    /// How many of the first `Command::SafetyNumber` asks are answered `SafetyUnavailable::NoKey`
    /// ("no key yet") before the first key is shown: a person whose key the fake learns late
    /// (0: the first ask has the number).
    pub keyless_asks: u32,
}

/// How the fake's upload phase of a message with files behaves ([`Script::uploads`]). The
/// reports follow the contract of `AttachmentProgress`: the first report names every file at
/// once, `total` is the file's size from then on, `done` never goes down and a file is full
/// only when its upload is over; the phase ends with `TransmitProgress { 1.0, None }`, and the
/// fake server takes the message after it.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadScript {
    /// The files upload one after another: the file being uploaded gains `step` of its size
    /// (a fraction, 0 < step <= 1) every 250 ms of the account clock, the files behind it are
    /// reported at 0. An abort during the phase is answered `Stopping`.
    Moving {
        /// The share of a file one tick adds.
        step: f32,
    },
    /// Each file is reported at its fixed share (`shares[i]` for file `i`, 0 for a file beyond
    /// the list; the reports are repeated every second of the account clock, as a ticker
    /// would, so a screen that opens late still hears them), and the phase lasts `hold` on the
    /// account clock. An abort during the phase is answered `Stopping`.
    Held {
        /// The fixed share of each file.
        shares: Vec<f32>,
        /// How long the phase lasts.
        hold: Duration,
    },
    /// The files are being made ready (a backend that re-encodes its pictures before the first
    /// byte): the phase `Preparing` is said (and again every second of the account clock, as
    /// a ticker would) for `hold` on the account clock and no file is reported; then every file
    /// is reported full and the phase ends. An abort during it is answered `Stopping`.
    Preparing {
        /// How long the preparation lasts.
        hold: Duration,
    },
    /// Like [`Held`](Self::Held), but the abort comes too late whenever it comes: it is
    /// answered `TooLate` and the held phase ends at once, the message goes out.
    TooLate {
        /// The fixed share of each file.
        shares: Vec<f32>,
        /// How long the phase lasts without an abort.
        hold: Duration,
    },
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
            pair_code: None,
            phone_login: None,
            steps: Vec::new(),
            after_connect: Vec::new(),
            reject_changes: false,
            roaming_writes: Some(true),
            forward_delay: None,
            transmit: Vec::new(),
            early_events: Vec::new(),
            catch_up_on_resume: None,
            reply_delays: Vec::new(),
            echo_without_binding: false,
            slow_sends: None,
            uploads: Vec::new(),
            challenge_answer: None,
            online_while_linking: false,
            with_ready: Vec::new(),
            fail_relinks: 0,
            download_delay: None,
            transfer: None,
            backfill: None,
            honour_number_tail: false,
            logout_unreachable: false,
            logout_backlog: 0,
            abort_backlog: 0,
            safety: Vec::new(),
            logout_delay: None,
            device_name: None,
            emoji_for: EMOJI_FOR,
            first_qr_for: None,
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

    /// States what the fake says of the roaming settings ([`roaming_writes`](Self::roaming_writes)).
    pub fn with_roaming_writes(mut self, roaming_writes: Option<bool>) -> Self {
        self.roaming_writes = roaming_writes;
        self
    }

    /// Gives a fresh link of the account a history transfer ([`transfer`](Self::transfer)).
    pub fn with_transfer(mut self, transfer: TransferScript) -> Self {
        self.transfer = Some(transfer);
        self
    }

    /// States how long the emoji step of a QR login stands ([`emoji_for`](Self::emoji_for)).
    pub fn with_emoji_for(mut self, emoji_for: Duration) -> Self {
        self.emoji_for = emoji_for;
        self
    }

    /// States how long the first code of the QR flow lives ([`first_qr_for`](Self::first_qr_for)).
    pub fn with_first_qr_for(mut self, first_qr_for: Duration) -> Self {
        self.first_qr_for = Some(first_qr_for);
        self
    }

    /// Gives the account a `pair-code` login flow ([`pair_code`](Self::pair_code)).
    pub fn with_pair_code(mut self, pair_code: PairCodeScript) -> Self {
        self.pair_code = Some(pair_code);
        self
    }

    /// Gives the account a `phone` login flow ([`phone_login`](Self::phone_login)).
    pub fn with_phone_login(mut self, phone_login: PhoneLoginScript) -> Self {
        self.phone_login = Some(phone_login);
        self
    }

    /// States `event` (ephemeral) at every connect, `delay` on the account clock after the
    /// runner brought the connection to `Online`, before the steps that are left run. A
    /// script whose next step is a `Step::Conn` runs the connection itself and gets none.
    pub fn after_connect(mut self, delay: Duration, event: BackendEvent) -> Self {
        self.after_connect.push((delay, event));
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

    /// The echo of an accepted message states no `allocated_as`
    /// ([`Self::echo_without_binding`]).
    pub fn with_echo_without_binding(mut self) -> Self {
        self.echo_without_binding = true;
        self
    }

    /// Every transmit reports `phase` and is answered `after` this long
    /// ([`Self::slow_sends`]).
    pub fn slow_sends(mut self, phase: Option<kourier_model::SendPhase>, after: Duration) -> Self {
        self.slow_sends = Some((phase, after));
        self
    }

    /// The actor sends `events` progress events before it answers an `AbortTransmit`
    /// ([`Self::abort_backlog`]).
    pub fn with_abort_backlog(mut self, events: usize) -> Self {
        self.abort_backlog = events;
        self
    }

    /// Gives the messages with files sent in `conv` an upload phase ([`Self::uploads`]).
    pub fn with_uploads(mut self, conv: ConvKey, uploads: UploadScript) -> Self {
        self.uploads.push((conv, uploads));
        self
    }

    /// Gives `user` safety numbers ([`Script::safety`]).
    pub fn with_safety(
        mut self,
        user: UserId,
        keys: Vec<(
            kourier_model::IdentityFingerprint,
            kourier_model::VerifyState,
        )>,
    ) -> Self {
        self.safety.push(SafetyScript {
            user,
            keys,
            keyless_asks: 0,
        });
        self
    }

    /// Gives `user` safety numbers, the first `keyless_asks` asks of which are answered "no key
    /// yet" ([`SafetyScript::keyless_asks`]).
    pub fn with_late_safety(
        mut self,
        user: UserId,
        keys: Vec<(
            kourier_model::IdentityFingerprint,
            kourier_model::VerifyState,
        )>,
        keyless_asks: u32,
    ) -> Self {
        self.safety.push(SafetyScript {
            user,
            keys,
            keyless_asks,
        });
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
