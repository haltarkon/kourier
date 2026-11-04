//! The login machine (W2, chunk C3): QR and phone-number linking, and the identity checks that
//! decide whether a pairing may stand.
//!
//! It works on [`WaSignal`]s (what `translate` made of the client's events) and on the person's
//! input, and it drives the client only through [`Session`]; it names nothing of whatsapp-rust. The
//! actor (C4) feeds it, appends what it hands back (`Out::Ready` is the `AccountReady` for the
//! core's outbox), and sends the steps to the core.
//!
//! - **Steps.** `step_id`s are K-A's vocabulary: `qr`, `phone`, `code`, `passkey`, `done` (plus
//!   `linking` for the progress between the pairing and the end); a failed step carries the step it
//!   failed at. `txn` rises with every step this login emits.
//! - **QR** (`flow "qr"`): one `ShowQr` per code the client rotates in; the codes running out is
//!   `Expired`; a phone without multi-device is `Unsupported`.
//! - **Phone number** (`flow "pair-code"`): an `Input` step for the number; the number is made
//!   digits-only and checked BEFORE a code is asked for (the code is pushed to that number's
//!   phone): another account's number, a relink to another number, the run tool's expected tail.
//!   Then the code is requested; upstream's refresh asks again with the same number, at most three
//!   times; a refusal of the request maps by its typed status (critic M7).
//! - **The pairing's checks** ([`Rules::decide`], the same function for the actor and for the
//!   Forwarder's [`Gate`]): the relink names the account, the id is not another account's of the
//!   profile, the number ends in the run tool's tail. A refusal ends in a `Failed` step and hands
//!   back [`Out::Refused`]; NOTHING is handed back for the core's outbox. The companion's logout
//!   is NOT awaited here (decision F3, W2a fix round): the event handler of the real client
//!   already started it the moment the gate refused, the actor picks its outcome up without
//!   blocking its loop ([`Login::logout_reached`]), and the core's `Logout`, which follows a
//!   refused link, reports it ([`Login::logout_report`]; `None` while it is pending).
//! - **The first line** (W2c, `crate::prepair`): the SAME rules also judge the `<pair-success>` STANZA, before upstream
//!   reads it and before a device exists ([`Gate::claim`]). A pairing refused there ends the login with the
//!   refusal's own `Failed` step and `Out::Stop` ([`Login::claimed`]), NOT `Out::Refused`: nothing was paired, so
//!   there is no logout to await and the core's `Logout` that follows finds no device. The gate also remembers the
//!   pairing it let through (a second phone's pairing is answered and goes no further, without the flag) and is
//!   closed when the login ends without a device ([`Gate::close`]).
//! - **Done** only after BOTH the core acked `AccountReady` ([`Login::acked`]) and the first
//!   `Connected` after the pairing came, in either order.
//!
//! Own ids are `[lid, pn]` (decision L4): the LID is the stable identity (a number change keeps
//! it), and the core's checks read the FIRST id.
//!
//! The number check before the code (review F1 of W2a, closed by W2b): the core hands `StartLogin.taken` as
//! EVERY own id of the other accounts (`ingest::other_identities`), so a WhatsApp account's phone number
//! is in the list next to its LID and [`Rules::check_number`] refuses a typed number of another account
//! BEFORE the code is asked for (a code would be pushed to that account's phone). The pairing's own check
//! ([`Rules::decide`], both ids) stays as the second line, for a number that changed since. The test
//! `link::tests::every_own_id_in_taken_refuses_a_number_before_the_code`
//! holds both halves.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kourier_backend_api::{Relink, TakenIdentity};
use kourier_model::{
    BackendError, DeviceLogout, ErrorKind, InputField, InputKind, LoginStep, LoginStepKind,
    Redacted, UserId,
};

use crate::session::{PairCodeFailure, Session};
use crate::translate::{PairRefusal, Paired, WaSignal};

/// The `step_id`s of a WhatsApp login (K-A keys its texts on them).
pub(crate) mod step {
    pub const QR: &str = "qr";
    pub const PHONE: &str = "phone";
    pub const CODE: &str = "code";
    /// Reserved for W9; W2 only refuses at it.
    pub const PASSKEY: &str = "passkey";
    pub const LINKING: &str = "linking";
    pub const DONE: &str = "done";
}

/// How many times an expired pair code is asked for again, with the same number, before the login
/// ends as `Expired` (critic M7).
pub(crate) const MAX_CODE_REFRESHES: u8 = 3;

/// The digits of a phone number with its country code: at least 7 (upstream's own bound), at most
/// 15 (E.164).
pub(crate) const NUMBER_DIGITS: std::ops::RangeInclusive<usize> = 7..=15;

/// Why a typed number is no number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumberProblem {
    /// A character that is neither a digit nor a separator.
    Characters,
    TooShort,
    TooLong,
    /// It starts with `0` after the international prefix is gone: the country code is missing
    /// (upstream refuses it too, and its server would never find the number).
    LeadingZero,
}

/// A typed phone number as digits. Separators are dropped (spaces, `-`, `.`, parentheses); one
/// leading `+` is allowed, and a leading `00` is the international prefix (`0049 151 ...` is
/// `49151...`) unless a `+` came before it.
pub(crate) fn normalize_number(typed: &str) -> Result<String, NumberProblem> {
    let trimmed = typed.trim();
    let (plus, rest) = match trimmed.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let mut digits = String::with_capacity(rest.len());
    for c in rest.chars() {
        match c {
            '0'..='9' => digits.push(c),
            '-' | '.' | '(' | ')' => {}
            c if c.is_whitespace() => {}
            _ => return Err(NumberProblem::Characters),
        }
    }
    let digits = if plus {
        digits
    } else {
        digits
            .strip_prefix("00")
            .map(str::to_owned)
            .unwrap_or(digits)
    };
    if digits.starts_with('0') {
        return Err(NumberProblem::LeadingZero);
    }
    match digits.len() {
        n if n < *NUMBER_DIGITS.start() => Err(NumberProblem::TooShort),
        n if n > *NUMBER_DIGITS.end() => Err(NumberProblem::TooLong),
        _ => Ok(digits),
    }
}

/// Why a link or a number was turned away. Every one ends the login with a `Failed` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The account is one of the profile's other accounts.
    AlreadyAdded { logged_out: bool },
    /// A relink brought another account than the one being relinked.
    NotTheRelinkedAccount,
    /// The run tool's expected tail does not match.
    NotTheExpectedNumber,
    /// The ids the phone sent could not be read as ids.
    Unreadable,
}

impl Refusal {
    /// The error of the `Failed` step. The detail is a fixed phrase; the user message names nobody.
    pub(crate) fn error(self) -> BackendError {
        match self {
            Self::AlreadyAdded { logged_out } => BackendError::new(
                ErrorKind::AlreadyAdded { logged_out },
                "this account is already in the profile",
            )
            .with_user_message("This account is already in Kourier."),
            Self::NotTheRelinkedAccount => BackendError::new(
                ErrorKind::InvalidInput,
                "the account is not the one being linked again",
            )
            .with_user_message("This is not the account that is being linked again."),
            Self::NotTheExpectedNumber => BackendError::new(
                ErrorKind::NotTheExpectedAccount,
                "the account is not the expected one",
            )
            .with_user_message("This is not the account Kourier was told to link."),
            Self::Unreadable => BackendError::new(
                ErrorKind::InvalidInput,
                "the phone sent an identity that could not be read",
            ),
        }
    }
}

/// What the profile and the run say about the account being linked. Immutable for a login.
#[derive(Debug, Clone)]
pub(crate) struct Rules {
    /// Every own id of the profile's other accounts (one entry per id).
    taken: Vec<TakenIdentity>,
    /// The ids of the account being relinked (empty for a first link, or when the core knows none:
    /// nothing is checked then, as the core does).
    expect_own: Vec<UserId>,
    relink: bool,
    /// The tail the run tool expects the number to end in (`BackendConfig::expect_number_suffix`).
    suffix: Option<Redacted<String>>,
}

impl Rules {
    pub(crate) fn new(
        taken: Vec<TakenIdentity>,
        relink: Option<Relink>,
        suffix: Option<Redacted<String>>,
    ) -> Self {
        Self {
            taken,
            relink: relink.is_some(),
            expect_own: relink.map(|r| r.expect_own).unwrap_or_default(),
            suffix,
        }
    }

    /// Whether this run checks the number's tail: the fixed token the log line and the
    /// development state say (`on` / `off`, never the digits). A run tool that set a tail
    /// reads `on` before it says "scan now": a tail that never reached the actor would let any
    /// phone that scans get the device (W2c, F2).
    pub(crate) fn number_check(&self) -> &'static str {
        if self.suffix.is_some() { "on" } else { "off" }
    }

    /// The checks on a typed number, BEFORE any code is asked for (nothing is linked yet, and the
    /// code would be pushed to that number's phone). `digits` is [`normalize_number`]'s output.
    pub(crate) fn check_number(&self, digits: &str) -> Result<(), Refusal> {
        if let Some(t) = self
            .taken
            .iter()
            .find(|t| t.user.scheme() == "pn" && t.user.value() == digits)
        {
            return Err(Refusal::AlreadyAdded {
                logged_out: t.logged_out,
            });
        }
        // A relink asks for the account's own number: a code pushed to a stranger's phone helps
        // nobody. An account with no number among its ids (a LID only) is not checked here; the
        // pairing's check below decides. After a number change the QR way is the way back.
        let own_pns: Vec<&UserId> = self
            .expect_own
            .iter()
            .filter(|u| u.scheme() == "pn")
            .collect();
        if self.relink && !own_pns.is_empty() && !own_pns.iter().any(|u| u.value() == digits) {
            return Err(Refusal::NotTheRelinkedAccount);
        }
        if !self.tail_matches(digits) {
            return Err(Refusal::NotTheExpectedNumber);
        }
        Ok(())
    }

    /// The checks on a finished pairing, in this order: the relink names the account, the id is not
    /// another account's, the number ends in the expected tail.
    pub(crate) fn decide(&self, paired: &Paired) -> Decision {
        let (Ok(pn), Ok(lid)) = (
            UserId::from_parts("pn", paired.pn.expose()),
            UserId::from_parts("lid", paired.lid.expose()),
        ) else {
            return Decision::Refuse(Refusal::Unreadable);
        };
        if self.relink
            && !self.expect_own.is_empty()
            && !self.expect_own.iter().any(|u| *u == pn || *u == lid)
        {
            return Decision::Refuse(Refusal::NotTheRelinkedAccount);
        }
        if let Some(t) = self.taken.iter().find(|t| t.user == pn || t.user == lid) {
            return Decision::Refuse(Refusal::AlreadyAdded {
                logged_out: t.logged_out,
            });
        }
        if !self.tail_matches(paired.pn.expose()) {
            return Decision::Refuse(Refusal::NotTheExpectedNumber);
        }
        Decision::Accept(Ready::of(pn, Some(lid), paired.device))
    }

    /// Whether `digits` ends in the expected tail (no tail set: any number). An empty tail fails
    /// closed, as Signal's does.
    fn tail_matches(&self, digits: &str) -> bool {
        match &self.suffix {
            None => true,
            Some(tail) => {
                let only = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
                let tail = only(tail.expose());
                !tail.is_empty() && only(digits).ends_with(&tail)
            }
        }
    }
}

/// What a finished pairing may become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Accept(Ready),
    Refuse(Refusal),
}

/// The account a pairing that passed the checks makes: what `AccountReady` carries. Its `Debug`
/// prints the id schemes and the device number only (review F5): the ids and the number are not for
/// a log line, and `Out` (which holds it) is printed by tests and by any `?outs` while debugging.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Ready {
    /// `[lid, pn]` (decision L4).
    pub own_ids: Vec<UserId>,
    /// This companion's device number.
    pub device: u16,
    /// `+<digits>`.
    pub number: Redacted<String>,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let schemes: Vec<&str> = self.own_ids.iter().map(UserId::scheme).collect();
        f.debug_struct("Ready")
            .field("own_ids", &schemes)
            .field("device", &self.device)
            .field("number", &"<hidden>")
            .finish()
    }
}

impl Ready {
    /// The account of `pn` and `lid` (an old account may have no LID): own ids `[lid, pn]`
    /// (decision L4), the number as `+<digits>`. The one place that decides the order, for a new
    /// pairing and for an `AccountReady` stated again from the stored device.
    pub(crate) fn of(pn: UserId, lid: Option<UserId>, device: u16) -> Self {
        let number = Redacted::new(format!("+{}", pn.value()));
        let own_ids = lid.into_iter().chain(std::iter::once(pn)).collect();
        Self {
            own_ids,
            device,
            number,
        }
    }
}

/// The Forwarder's synchronous half (critic H7, decision L6): it decides at the moment the
/// pairing's event is delivered, in the event handler, and the flag it sets makes the handler drop
/// every later event, so the client of a refused account never takes in its first sync or answers
/// a message. Shared between the Forwarder and the machine, which decide with the same rules.
///
/// W2c made it the FIRST line as well: the pre-pair claim (`prepair`) asks [`Gate::claim`] about the
/// `<pair-success>` stanza before upstream reads it, with the same rules. The gate remembers the one
/// identity it let through ([`GateState::accepted`]) and whether the login ended without a device
/// ([`Gate::close`]).
#[derive(Debug)]
pub(crate) struct Gate {
    rules: Rules,
    refused: AtomicBool,
    state: Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    /// The identity of the pairing that was let through: after it, the same identity may pass again
    /// (a redelivery), any other is answered and goes no further, and nothing sets `refused`.
    accepted: Option<Paired>,
    /// The login ended without a device (failed, cancelled, expired, withdrawn): nothing may pair now.
    closed: bool,
}

/// What the gate says about the `pair-success` children of a claimed `<iq>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Upstream may pair; the Forwarder's second line decides again on its event.
    Pass,
    /// Claim it and answer the pair error.
    Refuse(Refused),
}

/// A refused pairing: the one log word and whom to tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    /// A fixed word for the one log line: never an id, a JID, digits or the tail.
    pub token: &'static str,
    /// Whom to tell, after the answer left.
    pub tell: Tell,
}

/// Who learns of a refused pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Tell {
    /// Nobody: no login waits for it (none runs, it ended, a pairing already stands).
    Nobody,
    /// The login machine, with the refused ids (`None`: they could not be read).
    Login(Option<Paired>),
}

impl Refused {
    /// A pairing that arrives while no login can take it.
    pub(crate) fn no_login() -> Self {
        Self {
            token: "no-login",
            tell: Tell::Nobody,
        }
    }
}

impl Refusal {
    /// The fixed word of the one log line of a claimed refusal.
    fn token(self) -> &'static str {
        match self {
            Self::AlreadyAdded { .. } => "taken",
            Self::NotTheRelinkedAccount => "relink",
            Self::NotTheExpectedNumber => "expected-number",
            Self::Unreadable => "unreadable",
        }
    }
}

impl Gate {
    pub(crate) fn new(rules: Rules) -> Self {
        Self {
            rules,
            refused: AtomicBool::new(false),
            state: Mutex::new(GateState::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The rules this login decides by.
    pub(crate) fn rules(&self) -> &Rules {
        &self.rules
    }

    /// [`Rules::decide`] on a finished pairing (the second line), and a refusal sets the flag for
    /// good. An accepted one is remembered, so that the first line knows it.
    pub(crate) fn decide(&self, paired: &Paired) -> Decision {
        let decision = self.rules.decide(paired);
        match &decision {
            Decision::Refuse(_) => self.refused.store(true, Ordering::SeqCst),
            Decision::Accept(_) => {
                self.state().accepted.get_or_insert_with(|| paired.clone());
            }
        }
        decision
    }

    /// Whether a pairing was refused: every event after it is dropped.
    pub(crate) fn is_refused(&self) -> bool {
        self.refused.load(Ordering::SeqCst)
    }

    /// The login ended without a device: from now on every pairing is refused.
    pub(crate) fn close(&self) {
        self.state().closed = true;
    }

    /// Whether a pairing was let through (tests: a refusal must leave this unset).
    #[cfg(test)]
    pub(crate) fn has_accepted(&self) -> bool {
        self.state().accepted.is_some()
    }

    /// The first line (W2c): judges EVERY `pair-success` child of a claimed `<iq>` with the rules, before the
    /// device exists. All under one lock, so two pairings that arrive together cannot both pass or leave the
    /// flag set after one of them was let through.
    ///
    /// - The login is over without a device, or an earlier pairing was refused: refuse, tell nobody.
    /// - A pairing was already let through: the same identity passes again (a redelivery is the same values);
    ///   any other is refused WITHOUT setting the flag and WITHOUT telling the login (the account must not go
    ///   deaf, and its login must not fail, because a second phone scanned a rotated code).
    /// - Otherwise: a child the rules refuse, or that names no device, refuses (the flag is set and the login
    ///   is told); so does anything but exactly one child (whatsmeow requires exactly one, and upstream completes
    ///   the FIRST it handles, so a later child the rules accept must not hide an earlier stranger's).
    pub(crate) fn claim(&self, pairs: &[Option<Paired>]) -> Verdict {
        let mut state = self.state();
        if state.closed || self.is_refused() {
            return Verdict::Refuse(Refused::no_login());
        }
        if let Some(accepted) = &state.accepted {
            return match pairs {
                [Some(only)] if same_account(only, accepted) => Verdict::Pass,
                _ => Verdict::Refuse(Refused {
                    token: "second-pairing",
                    tell: Tell::Nobody,
                }),
            };
        }
        let mut found = None;
        for pair in pairs {
            let refusal = match pair {
                None => Some((Refusal::Unreadable, None)),
                Some(paired) => match self.rules.decide(paired) {
                    Decision::Refuse(refusal) => Some((refusal, Some(paired.clone()))),
                    Decision::Accept(_) => None,
                },
            };
            found = found.or(refusal);
        }
        match (found, pairs) {
            (Some((refusal, paired)), _) => self.refuse(refusal, paired),
            (None, [Some(only)]) => {
                state.accepted = Some(only.clone());
                Verdict::Pass
            }
            (None, _) => self.refuse(Refusal::Unreadable, None),
        }
    }

    fn refuse(&self, refusal: Refusal, paired: Option<Paired>) -> Verdict {
        self.refused.store(true, Ordering::SeqCst);
        Verdict::Refuse(Refused {
            token: refusal.token(),
            tell: Tell::Login(paired),
        })
    }
}

/// Whether two pairings name the same account (the device number is the server's, per pairing).
fn same_account(a: &Paired, b: &Paired) -> bool {
    a.pn.expose() == b.pn.expose() && a.lid.expose() == b.lid.expose()
}

/// What the machine hands to the actor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Out {
    /// A login step for the core.
    Step(LoginStep),
    /// The pairing passed its checks: append `AccountReady` to the core's outbox in one store zone
    /// and call [`Login::acked`] when the core acked it.
    Ready(Ready),
    /// The login is over without a device: stop the client (the shutdown order).
    Stop,
    /// The pairing was refused: the client must NOT be stopped yet, its logout is under way. The
    /// actor starts waiting for that logout's outcome in the background (the one shared answer of
    /// `Session::logout`), hands it to [`Login::logout_reached`] and stops the client then.
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Qr,
    Phone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// QR flow: the codes come as the client rotates them in.
    WaitingForScan,
    /// Phone flow: the `Input` step is shown.
    AwaitingNumber,
    /// Phone flow: a code was asked for (it is shown, or on its way).
    Code,
    /// The pairing stands: the core is told, the first `Connected` is awaited.
    Linking {
        acked: bool,
        connected: bool,
    },
    /// The pairing was refused. The companion's logout is under way (started by the client's event
    /// handler); `outcome` is what it reached, `None` while it is pending.
    Refused {
        outcome: Option<DeviceLogout>,
    },
    Done,
    /// Failed or cancelled: nothing more happens.
    Ended,
}

/// One login. Built by the actor at `StartLogin`, fed until [`Login::is_over`].
#[derive(Debug)]
pub(crate) struct Login {
    flow: Flow,
    gate: Arc<Gate>,
    stage: Stage,
    txn: u64,
    /// The `txn` of the `Input` step that `SubmitLoginInput` answers.
    input_txn: Option<u64>,
    /// The typed number as digits, kept for upstream's refresh.
    number: Option<Redacted<String>>,
    refreshes: u8,
}

impl Login {
    /// A login of the flow `flow` (`"qr"` or `"pair-code"`; anything else is unsupported).
    pub(crate) fn new(flow: &str, rules: Rules) -> Result<Self, BackendError> {
        let flow = match flow {
            "qr" => Flow::Qr,
            "pair-code" => Flow::Phone,
            _ => return Err(BackendError::unsupported("unknown login flow")),
        };
        Ok(Self {
            flow,
            gate: Arc::new(Gate::new(rules)),
            stage: Stage::WaitingForScan,
            txn: 0,
            input_txn: None,
            number: None,
            refreshes: 0,
        })
    }

    /// The client could not be started: the login ends at its first step.
    pub(crate) fn fail_start(&mut self, error: BackendError) -> Vec<Out> {
        let at = self.current_step();
        self.fail(at, error)
    }

    /// The Forwarder's half of the checks.
    pub(crate) fn gate(&self) -> Arc<Gate> {
        self.gate.clone()
    }

    /// The first steps: the number's `Input` for the phone flow, nothing for the QR (the client's
    /// codes arrive as signals).
    pub(crate) fn begin(&mut self) -> Vec<Out> {
        match self.flow {
            Flow::Qr => Vec::new(),
            Flow::Phone => {
                self.stage = Stage::AwaitingNumber;
                let txn = self.next_txn();
                self.input_txn = Some(txn);
                vec![self.step_at(
                    step::PHONE,
                    txn,
                    LoginStepKind::Input {
                        fields: vec![InputField {
                            id: "phone".to_owned(),
                            label: "Phone number".to_owned(),
                            kind: InputKind::Phone,
                            default: None,
                            required: true,
                            note: None,
                            code: None,
                        }],
                    },
                )]
            }
        }
    }

    /// The person's answer to the `Input` step `txn`. A stale or unexpected answer is ignored.
    pub(crate) async fn input(
        &mut self,
        session: &dyn Session,
        txn: u64,
        values: &[(String, Redacted<String>)],
    ) -> Vec<Out> {
        if self.stage != Stage::AwaitingNumber || self.input_txn != Some(txn) {
            return Vec::new();
        }
        let Some((_, typed)) = values.iter().find(|(id, _)| id == "phone") else {
            return self.fail(step::PHONE, invalid_number());
        };
        let Ok(digits) = normalize_number(typed.expose()) else {
            return self.fail(step::PHONE, invalid_number());
        };
        // Before the code is asked for: nothing is linked yet, and the code is pushed to that
        // number's phone.
        if let Err(refusal) = self.gate.rules().check_number(&digits) {
            return self.fail(step::PHONE, refusal.error());
        }
        self.number = Some(Redacted::new(digits.clone()));
        self.stage = Stage::Code;
        match session.pair_with_code(&digits).await {
            Ok(()) => Vec::new(),
            Err(failure) => self.code_failed(failure, step::PHONE),
        }
    }

    /// One signal of the client at `now_ms` (the wall clock, for a ban's end).
    pub(crate) async fn signal(
        &mut self,
        session: &dyn Session,
        signal: &WaSignal,
        now_ms: u64,
    ) -> Vec<Out> {
        if self.is_over() {
            return Vec::new();
        }
        match signal {
            WaSignal::QrCode { code, valid_for } => {
                if self.flow == Flow::Qr && self.stage == Stage::WaitingForScan {
                    let kind = LoginStepKind::ShowQr {
                        data: code.clone(),
                        valid_for: Some(*valid_for),
                    };
                    return vec![self.step(step::QR, kind)];
                }
                Vec::new()
            }
            WaSignal::QrExhausted { .. } => {
                if self.flow == Flow::Qr && self.stage == Stage::WaitingForScan {
                    return self.fail(
                        step::QR,
                        BackendError::new(ErrorKind::Expired, "the QR codes ran out"),
                    );
                }
                Vec::new()
            }
            WaSignal::QrScannedWithoutMultidevice => {
                if self.flow == Flow::Qr {
                    return self.fail(
                        step::QR,
                        BackendError::unsupported(
                            "the phone's WhatsApp does not support linked devices",
                        ),
                    );
                }
                Vec::new()
            }
            WaSignal::PairCode { code, .. } => {
                if self.flow == Flow::Phone && self.stage == Stage::Code {
                    let kind = LoginStepKind::ShowCode { code: code.clone() };
                    return vec![self.step(step::CODE, kind)];
                }
                Vec::new()
            }
            WaSignal::PairCodeRefresh { force_manual } => {
                if self.flow != Flow::Phone || self.stage != Stage::Code {
                    return Vec::new();
                }
                self.refresh(session, *force_manual).await
            }
            // The failure of a code request is the `Err` of the call that asked, which is handled
            // where it is made; the event only repeats it (upstream: "a direct caller sees the
            // failure twice"), so it is never a second report.
            WaSignal::PairCodeRefused { .. } => Vec::new(),
            WaSignal::PairSuccess(paired) => self.paired(paired),
            WaSignal::PairClaimed(paired) => self.claimed(paired.as_ref()),
            WaSignal::PairFailed => {
                let at = self.current_step();
                self.fail(
                    at,
                    BackendError::transient("the pairing's cryptography failed"),
                )
            }
            WaSignal::PasskeyRequested | WaSignal::PasskeyFailed { .. } => self.fail(
                step::PASSKEY,
                BackendError::unsupported(
                    "this account needs a passkey to link, which Kourier cannot do yet",
                ),
            ),
            WaSignal::PasskeyConfirmation { .. } => Vec::new(),
            WaSignal::Connected => self.connected(),
            // The connection-level verdicts end a login that has not finished: the actor shows
            // the connection state as well, but the link page must not wait for ever.
            WaSignal::Outdated => self.interrupted(BackendError::unsupported(
                "this version of Kourier is too old for WhatsApp",
            )),
            WaSignal::Banned(banned) => {
                let wait = Duration::from_millis(banned.until_ms.saturating_sub(now_ms));
                self.interrupted(BackendError::new(
                    ErrorKind::RateLimited {
                        retry_after: Some(wait),
                    },
                    "WhatsApp restricted the account",
                ))
            }
            WaSignal::Replaced | WaSignal::Refused { .. } => {
                self.interrupted(BackendError::transient("WhatsApp ended the connection"))
            }
            WaSignal::LoggedOut(_) => self.interrupted(BackendError::new(
                ErrorKind::LoggedOut,
                "WhatsApp logged the device out",
            )),
            // A write the account cannot go on without failed while the link was open: the client
            // is being ended. BEFORE the core acked `AccountReady` the link ends with it (the core
            // logs the device out; the person starts it again). AFTER the ack the account exists:
            // the failure is a lost connection like any other, the actor makes a new client after
            // a backoff and its `Connected` finishes the link (fix F8).
            WaSignal::WriteFailed => match self.stage {
                Stage::Linking { acked: true, .. } => Vec::new(),
                _ => self.interrupted(BackendError::transient("the account's database failed")),
            },
            WaSignal::Disconnected { .. }
            | WaSignal::WillRetry(_)
            | WaSignal::LocalLogout
            | WaSignal::Hold(_)
            | WaSignal::OfflinePreview { .. }
            | WaSignal::OfflineDone { .. }
            | WaSignal::OfflineInterrupted
            // Typing never reaches the login (the actor answers it before the machine).
            | WaSignal::Typing(_) => Vec::new(),
        }
    }

    /// Whether the receive path must not reach `core_outbox` yet (fix F5, `crate::hold`): the login
    /// has no account (before the pairing), or its pairing was refused and never will. Once the
    /// pairing is accepted the actor records `AccountReady` first and then opens the hold.
    pub(crate) fn holds_receive(&self) -> bool {
        matches!(
            self.stage,
            Stage::WaitingForScan | Stage::AwaitingNumber | Stage::Code | Stage::Refused { .. }
        )
    }

    /// The pairing stands and the core has acked `AccountReady`: only the first `Connected` is
    /// missing, so a client that ends meanwhile is made again and the link goes on (fix F8).
    pub(crate) fn is_linked(&self) -> bool {
        matches!(self.stage, Stage::Linking { acked: true, .. })
    }

    /// The core acked `AccountReady`.
    pub(crate) fn acked(&mut self) -> Vec<Out> {
        if let Stage::Linking { acked, .. } = &mut self.stage {
            *acked = true;
        }
        self.done_if_ready()
    }

    /// `CancelLogin`: withdraws a code request and ends the login. A pairing that already stands
    /// (or was refused) is not touched: the core's `Logout` that follows is the actor's to carry
    /// out.
    pub(crate) async fn cancel(&mut self, session: &dyn Session) -> Vec<Out> {
        match self.stage {
            Stage::WaitingForScan | Stage::AwaitingNumber | Stage::Code => {
                if self.flow == Flow::Phone {
                    session.cancel_pair_code().await;
                }
                self.stage = Stage::Ended;
                self.gate.close();
                vec![Out::Stop]
            }
            Stage::Linking { .. } | Stage::Refused { .. } | Stage::Done | Stage::Ended => {
                Vec::new()
            }
        }
    }

    /// Whether the pairing was refused (the login is kept until the core's `Logout` asks what the
    /// logout reached).
    pub(crate) fn is_refused(&self) -> bool {
        matches!(self.stage, Stage::Refused { .. })
    }

    /// What the logout of a refused pairing reached, for the `LogoutReport` when the core's
    /// `Logout` arrives. `None` when no pairing was refused or its logout is still pending.
    pub(crate) fn logout_report(&self) -> Option<DeviceLogout> {
        match self.stage {
            Stage::Refused { outcome } => outcome,
            _ => None,
        }
    }

    /// The logout of the refused pairing ended with `outcome` (the actor's pick-up). Only a
    /// confirmed removal is `Removed`; every other end is `NotReached`. The first answer is kept.
    pub(crate) fn logout_reached(&mut self, outcome: DeviceLogout) {
        if let Stage::Refused {
            outcome: slot @ None,
        } = &mut self.stage
        {
            *slot = Some(match outcome {
                DeviceLogout::Removed => DeviceLogout::Removed,
                _ => DeviceLogout::NotReached,
            });
        }
    }

    /// Nothing more will happen in this login.
    pub(crate) fn is_over(&self) -> bool {
        matches!(
            self.stage,
            Stage::Done | Stage::Ended | Stage::Refused { .. }
        )
    }

    /// The pairing finished: the checks, and then either the account or the refusal.
    fn paired(&mut self, paired: &Paired) -> Vec<Out> {
        if !matches!(
            self.stage,
            Stage::WaitingForScan | Stage::AwaitingNumber | Stage::Code
        ) {
            return Vec::new();
        }
        let at = self.current_step();
        match self.gate.decide(paired) {
            Decision::Accept(ready) => {
                self.stage = Stage::Linking {
                    acked: false,
                    connected: false,
                };
                let progress = self.step(
                    step::LINKING,
                    LoginStepKind::Progress {
                        label: "Linking".to_owned(),
                        fraction: None,
                    },
                );
                vec![Out::Ready(ready), progress]
            }
            Decision::Refuse(refusal) => {
                // The companion is on the phone's list already, and its logout was started by the
                // client's event handler the moment the gate refused. Nothing is awaited here: the
                // actor's loop must keep serving `Shutdown`, `CancelLogin` and the core's `Logout`
                // (review F6).
                let failed = self.step(
                    at,
                    LoginStepKind::Failed {
                        error: refusal.error(),
                    },
                );
                self.stage = Stage::Refused { outcome: None };
                vec![failed, Out::Refused]
            }
        }
    }

    /// The pre-pair claim refused a pairing before the device existed (W2c): the login ends with the
    /// refusal's own failure and the client is stopped. NOT `Out::Refused` and no `Stage::Refused`:
    /// nothing was paired, so there is no logout to wait for and the core's `Logout` that follows a
    /// failed link finds no device. The refusal is judged again here with the same rules, so the failure
    /// is the real cause; a claim whose ids pass (it cannot happen with the same rules) or that has none is
    /// unreadable. A claim before the number was typed (phone flow) fails at `code`, not at `phone`:
    /// the page would otherwise tell the person to check the number they never typed.
    fn claimed(&mut self, paired: Option<&Paired>) -> Vec<Out> {
        if !matches!(
            self.stage,
            Stage::WaitingForScan | Stage::AwaitingNumber | Stage::Code
        ) {
            return Vec::new();
        }
        let refusal = paired.map_or(Refusal::Unreadable, |paired| {
            match self.gate.rules().decide(paired) {
                Decision::Refuse(refusal) => refusal,
                Decision::Accept(_) => Refusal::Unreadable,
            }
        });
        let at = if self.stage == Stage::AwaitingNumber {
            step::CODE
        } else {
            self.current_step()
        };
        self.fail(at, refusal.error())
    }

    /// The first `Connected` after the pairing.
    fn connected(&mut self) -> Vec<Out> {
        if let Stage::Linking { connected, .. } = &mut self.stage {
            *connected = true;
        }
        self.done_if_ready()
    }

    fn done_if_ready(&mut self) -> Vec<Out> {
        if self.stage
            == (Stage::Linking {
                acked: true,
                connected: true,
            })
        {
            self.stage = Stage::Done;
            return vec![self.step(step::DONE, LoginStepKind::Done)];
        }
        Vec::new()
    }

    /// An expired pair code: asked for again with the same number, a few times.
    async fn refresh(&mut self, session: &dyn Session, force_manual: bool) -> Vec<Out> {
        if force_manual || self.refreshes >= MAX_CODE_REFRESHES {
            return self.fail(
                step::CODE,
                BackendError::new(ErrorKind::Expired, "the pair code expired"),
            );
        }
        let Some(number) = self.number.clone() else {
            return self.fail(step::CODE, BackendError::internal("no number to ask again"));
        };
        self.refreshes += 1;
        match session.pair_with_code(number.expose()).await {
            Ok(()) => Vec::new(),
            Err(failure) => self.code_failed(failure, step::CODE),
        }
    }

    /// A refused code request, by its typed status (critic M7). The 400 of the server is a
    /// malformed number or a throttle and cannot be told apart: with a back-off it is a wait,
    /// without one it is a number to check.
    fn code_failed(&mut self, failure: PairCodeFailure, at: &'static str) -> Vec<Out> {
        let error = match failure {
            // Not failures (upstream says so): a code is on screen, or the request was withdrawn.
            PairCodeFailure::AlreadyOutstanding | PairCodeFailure::Cancelled => {
                return Vec::new();
            }
            PairCodeFailure::InvalidNumber => invalid_number(),
            PairCodeFailure::NoAnswer => BackendError::transient("WhatsApp did not answer"),
            PairCodeFailure::Refused { code, backoff } => match PairRefusal::from_code(code) {
                PairRefusal::BadRequest if backoff.is_none() => BackendError::new(
                    ErrorKind::InvalidInput,
                    "WhatsApp refused the request as malformed",
                ),
                PairRefusal::BadRequest | PairRefusal::RateOverlimit => BackendError::new(
                    ErrorKind::RateLimited {
                        retry_after: backoff,
                    },
                    "WhatsApp asks to slow down",
                ),
                PairRefusal::NotAvailable => BackendError::unsupported(
                    "linking with a phone number is not available for this account",
                ),
                PairRefusal::Forbidden | PairRefusal::ServerError | PairRefusal::Other(_) => {
                    BackendError::new(
                        ErrorKind::Transient {
                            retry_after: backoff,
                        },
                        "WhatsApp could not make a code",
                    )
                }
            },
        };
        self.fail(at, error)
    }

    /// A terminal verdict of the connection before the login finished.
    fn interrupted(&mut self, error: BackendError) -> Vec<Out> {
        let at = self.current_step();
        self.fail(at, error)
    }

    /// The step the person is looking at.
    fn current_step(&self) -> &'static str {
        match (self.flow, self.stage) {
            (_, Stage::Linking { .. }) => step::LINKING,
            (Flow::Qr, _) => step::QR,
            (Flow::Phone, Stage::Code) => step::CODE,
            (Flow::Phone, _) => step::PHONE,
        }
    }

    /// Ends the login with a failed step at `at`, and stops the client. No device was made: from
    /// now on the gate refuses every pairing of this client.
    fn fail(&mut self, at: &str, error: BackendError) -> Vec<Out> {
        self.stage = Stage::Ended;
        self.gate.close();
        vec![self.step(at, LoginStepKind::Failed { error }), Out::Stop]
    }

    fn next_txn(&mut self) -> u64 {
        self.txn += 1;
        self.txn
    }

    fn step(&mut self, step_id: &str, kind: LoginStepKind) -> Out {
        let txn = self.next_txn();
        self.step_at(step_id, txn, kind)
    }

    fn step_at(&self, step_id: &str, txn: u64, kind: LoginStepKind) -> Out {
        Out::Step(LoginStep {
            step_id: step_id.to_owned(),
            txn,
            kind,
        })
    }
}

/// The error of a number that is no number.
fn invalid_number() -> BackendError {
    BackendError::new(ErrorKind::InvalidInput, "the phone number is not valid")
}

#[cfg(test)]
mod tests;
