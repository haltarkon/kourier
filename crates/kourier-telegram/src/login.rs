//! The login machine (TG-2 brief §3.1-§3.4, critic B1, M6, L7, M10): TDLib's authorization states
//! and the person's input in, Kourier's login steps and TDLib requests out. PURE: no I/O, no
//! clock, no async; the session (C8) feeds it and carries out what it hands back ([`Out`]).
//!
//! - **Steps.** `step_id`s are the shared vocabulary (`kourier_model::LoginStep::step_id`): `qr`,
//!   `phone`, `code`, `password`, `email`, `email-code`, `linking`, `done`, and the retry ids
//!   `code-again`, `password-again`, `email-again`, `email-code-again`. `txn` rises with EVERY
//!   step this login emits; an input whose `txn` is not the last step's is stale and ignored (L7),
//!   and one step's input is sent once.
//! - **A `Failed` step ENDS the login** (the core discards the account; "Try Again" is a new
//!   account with the same flow and the typed number, critic B1). So only TERMINAL outcomes are
//!   `Failed`: a refused identity, registration, a purchase, an invalid or banned number, any 429
//!   or `PHONE_NUMBER_FLOOD` (with its wait), an expired code, TDLib's own LOGOUT during the
//!   login (a close without one restarts, fix J5), the QR bound (M6), an update TDLib needs, a machine slip it cannot recover from. A
//!   wrong code, password, e-mail address or e-mail code shows the SAME input step again under
//!   its retry id (no new request, so no new code is sent). A helper death or a link timeout is
//!   [`Out::Abort`] with no step: the session restarts the helper, TDLib comes back in the state it
//!   persisted, and the machine shows that state's step again without a request. `Another
//!   authorization query has started` and `Call to .. unexpected` re-read the state
//!   (`getAuthorizationState`) and show it again.
//! - **QR first** (the descriptor's order, TSC M2): `requestQrCodeAuthentication` ONCE per login
//!   with the `tg:` ids of the profile's other Telegram accounts; every new link is a new `qr`
//!   step; the login ends `Expired` after [`QR_MAX_LINKS`] links or [`QR_BOUND_MS`] since the
//!   request, and a later `WaitPhoneNumber` in the QR flow is `Expired` too, never a second
//!   request (M6: an unattended QR page would otherwise ask Telegram every ~30 s for ever). The
//!   bound covers the WAIT FOR A SCAN only (fix J1): once TDLib leaves the QR state for a
//!   password, an e-mail step or `Ready` the person was scanned in and the bound is off. A helper
//!   that dies before the FIRST link allows ONE new request after its restart, the bound
//!   running on (fix J4: TDLib saves the QR state only once a token arrived).
//! - **Phone**: the typed number is made digits ([`crate::number::normalize`]) and checked BEFORE
//!   `setAuthenticationPhoneNumber` (the code would be sent to that number): another account's
//!   `pn:` id, a relink to another number, the run tool's expected tail ([`Rules::check_number`]).
//!   A code is resent only when TDLib offers a next type and its timeout has passed. A resend
//!   Telegram refuses (not a flood, not an expired code) leaves the code already sent valid: the
//!   code step comes again with no resend offered (fix J2).
//! - **His answers** (2026-10-25): the 2FA password with Telegram's own hint as the field's note
//!   (foreign text, M1), never a recovery or a password change (the recovery requests are
//!   forbidden in the helper); the e-mail steps done in Kourier ([`EMAIL_LOGIN`]), never Apple or
//!   Google sign-in; registration and purchases refused.
//! - **Ready**: `getMe`, then the SAME identity checks on both ids ([`Rules::decide`]); a refusal is
//!   a `Failed` step plus [`Out::Refused`] (a Telegram session exists now: the core's `Logout`
//!   that follows the failed step logs it out); an accepted identity is [`Out::Ready`] (the
//!   `AccountReady` for the core's outbox); `done` only after the core acked it ([`Login::acked`]).
//! - **One request in flight** (critic H1): the machine names the request it waits for; an input
//!   that comes meanwhile is held and sent when that answer came (at most one is held, the last).
//! - **Secrets** (M10): an input value is MOVED out of its `Redacted<String>` into a
//!   `Zeroizing<String>`, and a request's JSON is written by [`crate::link::secret_json`] from a
//!   borrowed struct into a zeroizing buffer; no `serde_json::Value` holds a typed value, nothing of
//!   it is kept after its request is built, and `Debug` of [`Out`] prints the request's kind only.

use std::fmt;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Relink, TakenIdentity};
use kourier_model::{
    BackendError, CodeDelivery, CodeResend, CodeVia, ErrorKind, InputField, InputKind, LoginStep,
    LoginStepKind, NetworkNote, Redacted, UserId,
};
use serde::Serialize;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::error::{LinkError, TdClass};
use crate::link::secret_json;
use crate::number;
use crate::parts::is_bidi_control;
use crate::tdapi::{AuthenticationCodeInfo, AuthorizationState, CodeType, User};

/// The `step_id`s of a Telegram login (K-A's vocabulary; KT-1b keys its words on them).
pub(crate) mod step {
    pub const QR: &str = "qr";
    pub const PHONE: &str = "phone";
    pub const CODE: &str = "code";
    pub const PASSWORD: &str = "password";
    pub const EMAIL: &str = "email";
    pub const EMAIL_CODE: &str = "email-code";
    pub const LINKING: &str = "linking";
    pub const DONE: &str = "done";
    /// The retry ids (critic B1): the same input step again after a wrong entry.
    pub const CODE_AGAIN: &str = "code-again";
    pub const PASSWORD_AGAIN: &str = "password-again";
    pub const EMAIL_AGAIN: &str = "email-again";
    pub const EMAIL_CODE_AGAIN: &str = "email-code-again";
    /// The FAILURE-only ids (KT-1b, `kourier_model::LoginStep::step_id`): a step Telegram asks for
    /// that Kourier does not do, so the login ends there, and the words are the UI's own. They are
    /// never an input step.
    pub const REGISTRATION: &str = "registration";
    pub const PURCHASE: &str = "purchase";
    pub const UPDATE: &str = "update";
}

/// The ids of the input fields (the keys of `SubmitLoginInput.values`).
pub(crate) mod field {
    pub const PHONE: &str = "phone";
    pub const CODE: &str = "code";
    /// Any value asks for the code again (a field KT-1b shows as a button): the model's one
    /// definition, which the UI sends.
    pub const RESEND: &str = kourier_model::RESEND_FIELD;
    pub const PASSWORD: &str = "password";
    pub const EMAIL: &str = "email";
}

/// His answer U-3 (2026-10-25, "Do it in Kourier"): a login e-mail Telegram asks for is typed in
/// Kourier (two more steps). `false` would refuse it with "finish this login in the Telegram app".
pub(crate) const EMAIL_LOGIN: bool = true;

/// The QR login's bound after its one request (critic M6).
pub(crate) const QR_BOUND_MS: u64 = 5 * 60 * 1000;
/// The most QR links one login shows (critic M6); the next one ends it `Expired`.
pub(crate) const QR_MAX_LINKS: u32 = 10;

/// A login flow of the descriptor (`descriptor.rs` `login_flows`, QR first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Qr,
    Phone,
}

impl Flow {
    /// The flow of a `StartLogin.flow`; `None` for an id the descriptor does not offer.
    pub(crate) fn parse(flow: &str) -> Option<Self> {
        match flow {
            "qr" => Some(Self::Qr),
            "phone" => Some(Self::Phone),
            _ => None,
        }
    }

    /// The step a failure before any step is reported at.
    pub(crate) fn first_step(self) -> &'static str {
        match self {
            Self::Qr => step::QR,
            Self::Phone => step::PHONE,
        }
    }
}

/// The requests the machine builds (their `@type`s are never one of the helper's forbidden
/// requests: test `no_request_the_machine_builds_is_forbidden`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReqKind {
    RequestQr,
    SetPhone,
    CheckCode,
    Resend,
    CheckPassword,
    SetEmail,
    CheckEmailCode,
    GetMe,
    GetState,
}

impl ReqKind {
    #[cfg(test)]
    pub(crate) const ALL: [ReqKind; 9] = [
        Self::RequestQr,
        Self::SetPhone,
        Self::CheckCode,
        Self::Resend,
        Self::CheckPassword,
        Self::SetEmail,
        Self::CheckEmailCode,
        Self::GetMe,
        Self::GetState,
    ];

    /// TDLib's name of the request (`td_api.tl`).
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            Self::RequestQr => "requestQrCodeAuthentication",
            Self::SetPhone => "setAuthenticationPhoneNumber",
            Self::CheckCode => "checkAuthenticationCode",
            Self::Resend => "resendAuthenticationCode",
            Self::CheckPassword => "checkAuthenticationPassword",
            Self::SetEmail => "setAuthenticationEmailAddress",
            Self::CheckEmailCode => "checkAuthenticationEmailCode",
            Self::GetMe => "getMe",
            Self::GetState => "getAuthorizationState",
        }
    }

    /// A request that carries what the person typed (or asked for): it answers one step.
    fn is_input(self) -> bool {
        match self {
            Self::SetPhone
            | Self::CheckCode
            | Self::Resend
            | Self::CheckPassword
            | Self::SetEmail
            | Self::CheckEmailCode => true,
            Self::RequestQr | Self::GetMe | Self::GetState => false,
        }
    }
}

/// A request for `HelperLink::request_secret`: its kind and its JSON (without `@extra`) in a
/// zeroizing buffer. `Debug` prints the kind only.
pub(crate) struct Request {
    pub(crate) kind: ReqKind,
    pub(crate) json: Zeroizing<String>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Request({:?})", self.kind)
    }
}

/// What the session must do, in order.
#[derive(Debug)]
pub(crate) enum Out {
    /// Send this step to the core (ephemeral `BackendEvent::Login`).
    Step(LoginStep),
    /// Send this request; its answer goes back through [`Login::on_answer`] with its kind.
    Request(Request),
    /// The helper died or did not answer: restart it (no step; TDLib comes back in the state it
    /// persisted and [`Login::on_state`] shows it again).
    Abort,
    /// An identity was refused AFTER TDLib logged it in: the session must be logged out (the
    /// `Failed` step came just before).
    Refused(Refusal),
    /// The identity was accepted: append its `AccountReady` to the outbox and call
    /// [`Login::acked`] once the core acked it.
    Ready(Ready),
}

/// Why an identity was turned away. Every one ends the login with a `Failed` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The account is one of the profile's other accounts.
    AlreadyAdded { logged_out: bool },
    /// A relink brought another account than the one being relinked.
    NotTheRelinkedAccount,
    /// The run tool's expected tail does not match.
    NotTheExpectedNumber,
    /// `getMe` did not answer a user Kourier can name.
    Unreadable,
}

impl Refusal {
    /// The error of the `Failed` step: fixed phrases, nobody named.
    pub(crate) fn error(self) -> BackendError {
        match self {
            Self::AlreadyAdded { logged_out } => BackendError::new(
                ErrorKind::AlreadyAdded { logged_out },
                "this Telegram account is already in the profile",
            ),
            Self::NotTheRelinkedAccount => BackendError::new(
                ErrorKind::InvalidInput,
                "the Telegram account is not the one being linked again",
            ),
            Self::NotTheExpectedNumber => BackendError::new(
                ErrorKind::NotTheExpectedAccount,
                "the Telegram account is not the expected one",
            ),
            Self::Unreadable => {
                BackendError::internal("Telegram named no account Kourier can read")
            }
        }
    }
}

/// An accepted identity: what `AccountReady` carries (L3: the display name too).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Ready {
    /// `[tg:<user id>, pn:<digits>]`, the primary first; `pn` only when TDLib gave a number.
    pub(crate) own_ids: Vec<UserId>,
    /// `+<digits>`.
    pub(crate) number: Option<Redacted<String>>,
    /// First and last name.
    pub(crate) display_name: Option<Redacted<String>>,
}

impl Ready {
    /// The `AccountReady` event of this identity (Telegram has no device id).
    pub(crate) fn event(&self) -> BackendEvent {
        BackendEvent::AccountReady {
            own_ids: self.own_ids.clone(),
            device: None,
            display_name: self.display_name.clone(),
            number: self.number.clone(),
        }
    }
}

/// What the profile and the run say about the account being linked. Immutable for a login.
#[derive(Debug, Clone)]
pub(crate) struct Rules {
    /// Every own id of the profile's other Telegram accounts (one entry per id).
    taken: Vec<TakenIdentity>,
    /// The ids of the account being relinked (empty: nothing to compare, as the core does).
    expect_own: Vec<UserId>,
    relink: bool,
    /// `BackendConfig::expect_number_suffix`.
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

    /// The `tg:` ids of the other accounts, as TDLib's int53 user ids (anything else is skipped).
    fn qr_ids(&self) -> Vec<i64> {
        self.taken
            .iter()
            .filter(|t| t.user.scheme() == "tg")
            .filter_map(|t| t.user.value().parse::<i64>().ok())
            .filter(|&id| id > 0)
            .collect()
    }

    /// Whether this run checks the number's tail: the fixed token the login-start log line says
    /// (`on` / `off`, never the digits), the same words as the fake's. A run tool that set a tail
    /// reads `on` before it says "type the number now": a tail that never reached the actor would
    /// let any account log in.
    pub(crate) fn number_check(&self) -> &'static str {
        if self.suffix.is_some() { "on" } else { "off" }
    }

    /// The checks on a typed number BEFORE `setAuthenticationPhoneNumber`. `digits` is
    /// [`number::normalize`]'s output.
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
        // A relink asks for the account's own number; an account with no `pn:` id is checked
        // after Ready only.
        let mut own_pns = self
            .expect_own
            .iter()
            .filter(|u| u.scheme() == "pn")
            .peekable();
        if self.relink && own_pns.peek().is_some() && !own_pns.any(|u| u.value() == digits) {
            return Err(Refusal::NotTheRelinkedAccount);
        }
        if !self.tail_matches(digits) {
            return Err(Refusal::NotTheExpectedNumber);
        }
        Ok(())
    }

    /// The checks on the logged-in user (`getMe`), in this order: the relink names the account,
    /// neither id is another account's, the number ends in the expected tail.
    pub(crate) fn decide(&self, user: &User) -> Result<Ready, Refusal> {
        if user.id <= 0 {
            return Err(Refusal::Unreadable);
        }
        let Ok(tg) = UserId::from_parts("tg", &user.id.to_string()) else {
            return Err(Refusal::Unreadable);
        };
        let digits: String = user
            .phone_number
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        let pn = if digits.is_empty() {
            None
        } else {
            match UserId::from_parts("pn", &digits) {
                Ok(pn) => Some(pn),
                Err(_) => return Err(Refusal::Unreadable),
            }
        };
        let is_own = |u: &UserId| *u == tg || pn.as_ref() == Some(u);
        if self.relink && !self.expect_own.is_empty() && !self.expect_own.iter().any(is_own) {
            return Err(Refusal::NotTheRelinkedAccount);
        }
        if let Some(t) = self.taken.iter().find(|t| is_own(&t.user)) {
            return Err(Refusal::AlreadyAdded {
                logged_out: t.logged_out,
            });
        }
        if !self.tail_matches(&digits) {
            return Err(Refusal::NotTheExpectedNumber);
        }
        // Foreign text (J13): cleaned and cut like the hint, the two names joined first.
        let mut both = String::with_capacity(user.first_name.len() + user.last_name.len() + 1);
        both.push_str(&user.first_name);
        both.push(' ');
        both.push_str(&user.last_name);
        let name = plain_line(&both).unwrap_or_default();
        // Built by hand, not by `format!` (no number goes through a formatting call).
        let number = (!digits.is_empty()).then(|| {
            let mut plus = String::with_capacity(digits.len() + 1);
            plus.push('+');
            plus.push_str(&digits);
            Redacted::new(plus)
        });
        let mut own_ids = vec![tg];
        own_ids.extend(pn);
        Ok(Ready {
            own_ids,
            number,
            display_name: (!name.is_empty()).then(|| Redacted::new(name)),
        })
    }

    /// Whether `digits` ends in the expected tail (no tail: any number). An empty tail fails
    /// closed, as WhatsApp's and Signal's do.
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

/// The code step as shown: its label, how the code was sent and when a resend is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CodeShown {
    label: &'static str,
    /// How the code was sent (the typed delivery the UI words).
    via: CodeVia,
    /// The way a new code would come: TDLib named a next type Kourier can take. `None`: no next
    /// type, one Kourier cannot take (a flash call, a missed call, Firebase: a resend into it
    /// would end the login), or a resend Telegram refused (J2).
    resend: Option<CodeVia>,
    /// The first letter or word of an SMS that sends one (TDLib's `first_letter` / `first_word`),
    /// as the field's note: foreign text, one plain line, never logged.
    hint: Option<NetworkNote>,
    timeout_ms: u64,
    since_ms: u64,
}

impl CodeShown {
    /// When a new code may be asked for (`since + timeout`), in the caller's milliseconds.
    fn due_ms(&self) -> u64 {
        self.since_ms.saturating_add(self.timeout_ms)
    }

    /// What the code field states at `now_ms`: the way the code came and, when a new one may come,
    /// the wait that REMAINS (0: at once; every showing of the step restates what is left).
    fn delivery(&self, now_ms: u64) -> CodeDelivery {
        CodeDelivery {
            via: self.via,
            resend: self.resend.map(|via| CodeResend {
                after: Duration::from_millis(self.due_ms().saturating_sub(now_ms)),
                via,
            }),
        }
    }
}

/// How Telegram's `type` of a code is worded for the UI; `None` for a type Kourier cannot take
/// (never asked for: Kourier sets no flash or missed call and is no official app).
fn via_of(ty: &CodeType) -> Option<CodeVia> {
    match ty {
        CodeType::TelegramMessage { .. } => Some(CodeVia::App),
        CodeType::Sms { .. } => Some(CodeVia::Sms),
        CodeType::SmsWord { .. } => Some(CodeVia::SmsWord),
        CodeType::SmsPhrase { .. } => Some(CodeVia::SmsPhrase),
        CodeType::Call { .. } => Some(CodeVia::Call),
        CodeType::Fragment { .. } => Some(CodeVia::Fragment),
        CodeType::Unknown => Some(CodeVia::Other),
        CodeType::FlashCall
        | CodeType::MissedCall { .. }
        | CodeType::FirebaseAndroid
        | CodeType::FirebaseIos => None,
    }
}

/// The step on screen (what an input answers, what a retry shows again).
#[derive(Debug, Clone, PartialEq)]
enum Shown {
    Nothing,
    Qr,
    Phone,
    Code(CodeShown),
    Password { note: Option<NetworkNote> },
    Email,
    EmailCode,
    Linking,
    Done,
}

/// One login of one account actor.
#[derive(Debug)]
pub(crate) struct Login {
    flow: Flow,
    rules: Rules,
    /// The `txn` of the last step emitted (0: none yet).
    txn: u64,
    shown: Shown,
    /// A `Failed` or `Done` step was emitted: nothing more comes out.
    ended: bool,
    /// Kourier asked TDLib to log out (a `CancelLogin`'s logout, L5): TDLib's LoggingOut and
    /// Closed are then no failure.
    logout_asked: bool,
    /// The request whose answer is awaited (at most one, H1).
    pending: Option<ReqKind>,
    /// One request held until `pending` is answered.
    queued: Option<Request>,
    /// The `txn` whose input was already sent (one step's input is sent once).
    sent_txn: Option<u64>,
    /// `requestQrCodeAuthentication` was built (once per login, M6).
    qr_requested: bool,
    /// When the QR request was built; `None` before it and once the QR was scanned (J1).
    qr_since_ms: Option<u64>,
    qr_links: u32,
    /// TDLib left the QR state for a password, an e-mail step or `Ready` (J1): nothing waits for
    /// a scan any more, so no bound runs.
    qr_scanned: bool,
    /// A helper that died before the first link got its one new request (J4).
    qr_retried: bool,
    /// `authorizationStateReady` was seen (`getMe` asked once).
    ready_seen: bool,
    /// `Out::Ready` was handed out; `done` waits for [`Login::acked`].
    awaiting_ack: bool,
}

impl Login {
    pub(crate) fn new(flow: Flow, rules: Rules) -> Self {
        Self {
            flow,
            rules,
            txn: 0,
            shown: Shown::Nothing,
            ended: false,
            logout_asked: false,
            pending: None,
            queued: None,
            sent_txn: None,
            qr_requested: false,
            qr_since_ms: None,
            qr_links: 0,
            qr_scanned: false,
            qr_retried: false,
            ready_seen: false,
            awaiting_ack: false,
        }
    }

    /// The `txn` of the last step emitted.
    #[cfg(test)]
    pub(crate) fn txn(&self) -> u64 {
        self.txn
    }

    /// A `Failed` or `Done` step was emitted.
    pub(crate) fn is_ended(&self) -> bool {
        self.ended
    }

    /// The request whose answer the machine waits for.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> Option<ReqKind> {
        self.pending
    }

    /// The step a failure now is reported at.
    pub(crate) fn step_id(&self) -> &'static str {
        match self.shown {
            Shown::Nothing => self.flow.first_step(),
            Shown::Qr => step::QR,
            Shown::Phone => step::PHONE,
            Shown::Code(_) => step::CODE,
            Shown::Password { .. } => step::PASSWORD,
            Shown::Email => step::EMAIL,
            Shown::EmailCode => step::EMAIL_CODE,
            Shown::Linking => step::LINKING,
            Shown::Done => step::DONE,
        }
    }

    /// Ends the login with `error` at the current step (the actor's own terminal failures: no
    /// API key, no library, the restart budget used up).
    pub(crate) fn fail(&mut self, error: BackendError) -> Vec<Out> {
        if self.ended {
            return Vec::new();
        }
        let id = self.step_id();
        self.failed(id, error)
    }

    /// Kourier asked TDLib to log out (L5).
    pub(crate) fn logout_asked(&mut self) {
        self.logout_asked = true;
    }

    /// The helper was restarted (C8): the request in flight and a held one went with the old
    /// helper (their answers never come). TDLib comes back in the state it persisted and
    /// [`Login::on_state`] shows that step again. A `Ready` whose `getMe` was not answered yet asks
    /// again; once `Out::Ready` was handed out (the `AccountReady` is in the outbox) it does not.
    ///
    /// A QR login whose helper died before its FIRST link may ask once more (J4): TDLib saves the
    /// QR state only when a token arrived, so it comes back in `WaitPhoneNumber`. The request time
    /// is kept, so the five-minute bound still covers the whole login.
    pub(crate) fn on_restart(&mut self) {
        self.pending = None;
        self.queued = None;
        if !self.awaiting_ack {
            self.ready_seen = false;
        }
        if self.flow == Flow::Qr
            && self.qr_requested
            && self.qr_links == 0
            && !self.qr_retried
            && !self.qr_scanned
        {
            self.qr_requested = false;
            self.qr_retried = true;
        }
    }

    /// TDLib left the QR state for a step after the scan (J1): the bound ends, for good.
    fn scanned(&mut self) {
        self.qr_scanned = true;
        self.qr_since_ms = None;
    }

    /// When [`Login::on_tick`] must be called next (the QR bound), in the caller's milliseconds.
    pub(crate) fn deadline_ms(&self) -> Option<u64> {
        if self.ended || self.ready_seen {
            return None;
        }
        self.qr_since_ms
            .map(|since| since.saturating_add(QR_BOUND_MS))
    }

    /// The QR bound (M6).
    pub(crate) fn on_tick(&mut self, now_ms: u64) -> Vec<Out> {
        match self.deadline_ms() {
            Some(at) if now_ms >= at => {
                self.failed(step::QR, BackendError::new(ErrorKind::Expired, QR_ENDED))
            }
            _ => Vec::new(),
        }
    }

    /// An authorization state of TDLib (an update, a replayed first state, a re-read).
    pub(crate) fn on_state(&mut self, state: AuthorizationState, now_ms: u64) -> Vec<Out> {
        if self.ended {
            return Vec::new();
        }
        match state {
            AuthorizationState::WaitTdlibParameters | AuthorizationState::Unknown => Vec::new(),
            AuthorizationState::WaitPhoneNumber => match self.flow {
                Flow::Qr if !self.qr_requested => {
                    self.qr_requested = true;
                    // The first request starts the bound; the one new request after a helper
                    // death (J4) keeps it.
                    self.qr_since_ms.get_or_insert(now_ms);
                    let ids = self.rules.qr_ids();
                    let req = request(
                        ReqKind::RequestQr,
                        &RequestQr {
                            ty: ReqKind::RequestQr.type_name(),
                            other_user_ids: &ids,
                        },
                    );
                    self.issue(req).into_iter().collect()
                }
                // A second WaitPhoneNumber in the QR flow (an error cleared TDLib's state, the
                // token could not be renewed): never a second request (M6).
                Flow::Qr => self.failed(step::QR, BackendError::new(ErrorKind::Expired, QR_ENDED)),
                Flow::Phone => {
                    self.shown = Shown::Phone;
                    self.input_step(false, now_ms).into_iter().collect()
                }
            },
            AuthorizationState::WaitOtherDeviceConfirmation { link } => {
                // A link means a request is in effect (J4).
                self.qr_requested = true;
                let late = !self.qr_scanned && {
                    let since = *self.qr_since_ms.get_or_insert(now_ms);
                    now_ms >= since.saturating_add(QR_BOUND_MS)
                };
                if self.qr_links >= QR_MAX_LINKS || late {
                    return self.failed(step::QR, BackendError::new(ErrorKind::Expired, QR_ENDED));
                }
                self.qr_links += 1;
                self.shown = Shown::Qr;
                vec![self.step(
                    step::QR,
                    LoginStepKind::ShowQr {
                        data: Redacted::new(link),
                        valid_for: None,
                    },
                )]
            }
            AuthorizationState::WaitCode { code_info } => self.code_state(&code_info, now_ms),
            AuthorizationState::WaitPassword { password_hint, .. } => {
                self.scanned();
                self.shown = Shown::Password {
                    note: note_of(&password_hint),
                };
                self.input_step(false, now_ms).into_iter().collect()
            }
            AuthorizationState::WaitEmailAddress { .. } if EMAIL_LOGIN => {
                self.scanned();
                self.shown = Shown::Email;
                self.input_step(false, now_ms).into_iter().collect()
            }
            AuthorizationState::WaitEmailAddress { .. } => self.failed(
                step::EMAIL,
                BackendError::unsupported("Telegram asks for a login e-mail address"),
            ),
            AuthorizationState::WaitEmailCode { .. } => {
                self.scanned();
                self.shown = Shown::EmailCode;
                self.input_step(false, now_ms).into_iter().collect()
            }
            AuthorizationState::WaitRegistration => self.failed(
                step::REGISTRATION,
                BackendError::unsupported("this number has no Telegram account"),
            ),
            AuthorizationState::WaitPremiumPurchase => self.failed(
                step::PURCHASE,
                BackendError::unsupported("Telegram asks for a purchase to log in"),
            ),
            AuthorizationState::Ready if self.ready_seen => Vec::new(),
            AuthorizationState::Ready => {
                self.scanned();
                self.ready_seen = true;
                self.shown = Shown::Linking;
                let mut outs = vec![self.step(
                    step::LINKING,
                    LoginStepKind::Progress {
                        label: "Linking…".to_owned(),
                        fraction: None,
                    },
                )];
                let req = request(
                    ReqKind::GetMe,
                    &Typed {
                        ty: ReqKind::GetMe.type_name(),
                    },
                );
                outs.extend(self.issue(req));
                outs
            }
            // Kourier's own `logOut` (L5): TDLib's LoggingOut, Closing and Closed are no end.
            AuthorizationState::LoggingOut
            | AuthorizationState::Closing
            | AuthorizationState::Closed
                if self.logout_asked =>
            {
                Vec::new()
            }
            // Only TDLib's own LOGOUT ends a login (B1, J5).
            AuthorizationState::LoggingOut => {
                let id = self.step_id();
                self.failed(id, BackendError::transient("Telegram ended the login"))
            }
            // A client that closes without a logout is a restart, as a linked session's is: TDLib
            // comes back in the state it persisted (J5).
            AuthorizationState::Closing | AuthorizationState::Closed => vec![Out::Abort],
        }
    }

    /// The person's answer to the step `txn` (`SubmitLoginInput`). A stale `txn`, a step whose
    /// input was already sent and an ended login are ignored.
    ///
    /// Every typed value that is not moved into a request is zeroed when it goes (J14, [`wipe`]).
    pub(crate) fn on_input(
        &mut self,
        txn: u64,
        mut values: Vec<(String, Redacted<String>)>,
        now_ms: u64,
    ) -> Vec<Out> {
        let outs = self.input(txn, &mut values, now_ms);
        wipe(values);
        outs
    }

    fn input(
        &mut self,
        txn: u64,
        values: &mut Vec<(String, Redacted<String>)>,
        now_ms: u64,
    ) -> Vec<Out> {
        if self.ended || txn != self.txn || self.sent_txn == Some(txn) {
            return Vec::new();
        }
        let req = match self.shown.clone() {
            Shown::Phone => {
                let Some(typed) = take(values, field::PHONE) else {
                    return self.input_step(false, now_ms).into_iter().collect();
                };
                let digits = match number::normalize(&typed) {
                    Ok(d) => d,
                    Err(_) => {
                        return self.failed(
                            step::PHONE,
                            BackendError::new(
                                ErrorKind::InvalidInput,
                                "the typed text is not a phone number",
                            ),
                        );
                    }
                };
                drop(typed);
                if let Err(refusal) = self.rules.check_number(&digits) {
                    return self.failed(step::PHONE, refusal.error());
                }
                let mut plus: Zeroizing<String> =
                    Zeroizing::new(String::with_capacity(digits.len() + 1));
                plus.push('+');
                plus.push_str(&digits);
                request(
                    ReqKind::SetPhone,
                    &SetPhone {
                        ty: ReqKind::SetPhone.type_name(),
                        phone_number: &plus,
                        settings: PhoneSettings::NONE,
                    },
                )
            }
            Shown::Code(code) => {
                if take(values, field::RESEND).is_some() {
                    if code.resend.is_none() || now_ms < code.due_ms() {
                        // Telegram does not allow it (yet): the same step again, nothing sent.
                        return self.input_step(false, now_ms).into_iter().collect();
                    }
                    request(
                        ReqKind::Resend,
                        &Resend {
                            ty: ReqKind::Resend.type_name(),
                            reason: Typed {
                                ty: "resendCodeReasonUserRequest",
                            },
                        },
                    )
                } else {
                    let Some(typed) = take(values, field::CODE) else {
                        return self.input_step(false, now_ms).into_iter().collect();
                    };
                    request(
                        ReqKind::CheckCode,
                        &CheckCode {
                            ty: ReqKind::CheckCode.type_name(),
                            code: &typed,
                        },
                    )
                }
            }
            Shown::Password { .. } => {
                let Some(typed) = take(values, field::PASSWORD) else {
                    return self.input_step(false, now_ms).into_iter().collect();
                };
                request(
                    ReqKind::CheckPassword,
                    &CheckPassword {
                        ty: ReqKind::CheckPassword.type_name(),
                        password: &typed,
                    },
                )
            }
            Shown::Email => {
                let Some(typed) = take(values, field::EMAIL) else {
                    return self.input_step(false, now_ms).into_iter().collect();
                };
                request(
                    ReqKind::SetEmail,
                    &SetEmail {
                        ty: ReqKind::SetEmail.type_name(),
                        email_address: &typed,
                    },
                )
            }
            Shown::EmailCode => {
                let Some(typed) = take(values, field::CODE) else {
                    return self.input_step(false, now_ms).into_iter().collect();
                };
                request(
                    ReqKind::CheckEmailCode,
                    &CheckEmailCode {
                        ty: ReqKind::CheckEmailCode.type_name(),
                        code: EmailCode {
                            ty: "emailAddressAuthenticationCode",
                            code: &typed,
                        },
                    },
                )
            }
            Shown::Nothing | Shown::Qr | Shown::Linking | Shown::Done => return Vec::new(),
        };
        self.sent_txn = Some(txn);
        self.issue(req).into_iter().collect()
    }

    /// The answer to a request this machine built (`kind` as handed out). An answer of a kind
    /// that is not the awaited one is stale and ignored.
    pub(crate) fn on_answer(
        &mut self,
        kind: ReqKind,
        result: Result<Value, LinkError>,
        now_ms: u64,
    ) -> Vec<Out> {
        if self.pending != Some(kind) {
            return Vec::new();
        }
        self.pending = None;
        let mut outs = if self.ended {
            Vec::new()
        } else {
            match result {
                Ok(v) => self.on_ok(kind, v, now_ms),
                Err(e) => self.on_error(kind, e, now_ms),
            }
        };
        if self.ended {
            self.queued = None;
        } else if self.pending.is_none()
            && let Some(next) = self.queued.take()
        {
            outs.extend(self.issue(next));
        }
        outs
    }

    /// The core acked the `AccountReady` of [`Out::Ready`]: the login is done.
    pub(crate) fn acked(&mut self) -> Vec<Out> {
        if self.ended || !self.awaiting_ack {
            return Vec::new();
        }
        self.awaiting_ack = false;
        self.shown = Shown::Done;
        self.ended = true;
        vec![self.step(step::DONE, LoginStepKind::Done)]
    }

    fn on_ok(&mut self, kind: ReqKind, v: Value, now_ms: u64) -> Vec<Out> {
        match kind {
            ReqKind::GetMe => match User::from_value(v) {
                None => self.refused(Refusal::Unreadable),
                Some(user) => match self.rules.decide(&user) {
                    Err(refusal) => self.refused(refusal),
                    Ok(ready) => {
                        self.awaiting_ack = true;
                        vec![Out::Ready(ready)]
                    }
                },
            },
            ReqKind::GetState => self.on_state(AuthorizationState::from_value(v), now_ms),
            // `ok`: the state that follows is an update of its own.
            ReqKind::RequestQr
            | ReqKind::SetPhone
            | ReqKind::CheckCode
            | ReqKind::Resend
            | ReqKind::CheckPassword
            | ReqKind::SetEmail
            | ReqKind::CheckEmailCode => Vec::new(),
        }
    }

    fn on_error(&mut self, kind: ReqKind, e: LinkError, now_ms: u64) -> Vec<Out> {
        if kind == ReqKind::Resend && is_refused_resend(&e) {
            return self.resend_refused(now_ms);
        }
        match e {
            LinkError::Auth {
                class,
                retry_after_s,
                ..
            } => match class {
                TdClass::TooManyRequests | TdClass::PhoneNumberFlood => {
                    let id = self.step_id();
                    let retry_after = retry_after_s.map(|s| Duration::from_secs(u64::from(s)));
                    self.failed(
                        id,
                        BackendError::new(
                            ErrorKind::RateLimited { retry_after },
                            "Telegram asks to wait before the next try",
                        ),
                    )
                }
                TdClass::PhoneNumberInvalid => self.failed(
                    step::PHONE,
                    BackendError::new(ErrorKind::InvalidInput, "Telegram refused the number"),
                ),
                TdClass::PhoneNumberBanned => self.failed(
                    step::PHONE,
                    BackendError::new(ErrorKind::Unauthorized, "Telegram banned the number"),
                ),
                TdClass::PhoneCodeExpired => self.failed(
                    step::CODE,
                    BackendError::new(ErrorKind::Expired, "the code expired"),
                ),
                TdClass::EmailNotAllowed => self.failed(
                    step::EMAIL,
                    BackendError::unsupported("Telegram does not take this e-mail address"),
                ),
                TdClass::UpdateNeeded => self.failed(
                    step::UPDATE,
                    BackendError::unsupported("Telegram asks for a newer application"),
                ),
                TdClass::PhoneCodeInvalid
                | TdClass::PhoneCodeEmpty
                | TdClass::CodeInvalid
                | TdClass::PasswordHashInvalid
                | TdClass::InvalidRecoveryCode
                | TdClass::EmailInvalid
                | TdClass::EmailCodeInvalid => self.retry(kind, now_ms),
                TdClass::AnotherQuery | TdClass::Unexpected | TdClass::FrozenMethodInvalid => {
                    self.reread()
                }
            },
            // The helper is gone or did not answer: no step (B1); the session restarts it.
            LinkError::Timeout
            | LinkError::Closed
            | LinkError::HelloTimeout
            | LinkError::DatabaseLocked => vec![Out::Abort],
            LinkError::Frame
            | LinkError::VersionMismatch
            | LinkError::Refused(_)
            | LinkError::Td { .. } => {
                let id = self.step_id();
                self.failed(
                    id,
                    BackendError::internal("Telegram's answer to a login request was not expected"),
                )
            }
        }
    }

    /// Telegram refused a resend (J2): TDLib stays in `WaitCode` and the code already sent still
    /// works, so the login goes on. The code step comes again (txn + 1, no request), with no
    /// resend offered.
    fn resend_refused(&mut self, now_ms: u64) -> Vec<Out> {
        match &mut self.shown {
            Shown::Code(code) => {
                code.resend = None;
                self.input_step(false, now_ms).into_iter().collect()
            }
            _ => Vec::new(),
        }
    }

    /// A wrong entry: the SAME input step again under its retry id, when the request answered
    /// the step on screen; otherwise the state is read again.
    fn retry(&mut self, kind: ReqKind, now_ms: u64) -> Vec<Out> {
        let answers_shown = matches!(
            (kind, &self.shown),
            (ReqKind::CheckCode, Shown::Code(_))
                | (ReqKind::CheckPassword, Shown::Password { .. })
                | (ReqKind::SetEmail, Shown::Email)
                | (ReqKind::CheckEmailCode, Shown::EmailCode)
        );
        if answers_shown {
            self.input_step(true, now_ms).into_iter().collect()
        } else {
            self.reread()
        }
    }

    /// `getAuthorizationState`, then the state's step again.
    fn reread(&mut self) -> Vec<Out> {
        let req = request(
            ReqKind::GetState,
            &Typed {
                ty: ReqKind::GetState.type_name(),
            },
        );
        self.issue(req).into_iter().collect()
    }

    fn code_state(&mut self, info: &AuthenticationCodeInfo, now_ms: u64) -> Vec<Out> {
        // How the code came. A type Kourier cannot take (never asked for: Kourier sets no flash
        // or missed call and is no official app) ends the login.
        let Some(via) = via_of(&info.r#type) else {
            return self.failed(
                step::CODE,
                BackendError::unsupported("Telegram sent the code a way Kourier cannot take"),
            );
        };
        // The English fallback label of the field (a UI without words); the UI words the
        // delivery itself.
        let label = match via {
            CodeVia::App => "Code (sent to your Telegram app)",
            CodeVia::Sms | CodeVia::SmsWord | CodeVia::SmsPhrase => "Code (sent by SMS)",
            CodeVia::Call => "Code (from the call)",
            CodeVia::Fragment => "Code (sent to fragment.com)",
            CodeVia::Other => "Code",
        };
        // The first letter or word of an SMS that sends one is the field's note.
        let hint = match &info.r#type {
            CodeType::SmsWord { first_letter } => note_of(first_letter),
            CodeType::SmsPhrase { first_word } => note_of(first_word),
            _ => None,
        };
        self.shown = Shown::Code(CodeShown {
            label,
            via,
            // The way a new code would come: only one Kourier can take (a resend into a flash
            // call, a missed call or Firebase would end the login).
            resend: info.next_type.as_ref().and_then(via_of),
            hint,
            timeout_ms: u64::try_from(info.timeout.max(0)).unwrap_or(0) * 1000,
            since_ms: now_ms,
        });
        self.input_step(false, now_ms).into_iter().collect()
    }

    /// The input step of what is on screen; `again`: under its retry id.
    fn input_step(&mut self, again: bool, now_ms: u64) -> Option<Out> {
        let (id, again_id, fields) = match &self.shown {
            Shown::Phone => (
                step::PHONE,
                step::PHONE,
                vec![input(field::PHONE, "Phone number", InputKind::Phone, None)],
            ),
            Shown::Code(code) => (
                step::CODE,
                step::CODE_AGAIN,
                vec![InputField {
                    code: Some(code.delivery(now_ms)),
                    ..input(field::CODE, code.label, InputKind::Text, code.hint.clone())
                }],
            ),
            Shown::Password { note } => (
                step::PASSWORD,
                step::PASSWORD_AGAIN,
                vec![input(
                    field::PASSWORD,
                    "Password (two-step verification)",
                    InputKind::Secret,
                    note.clone(),
                )],
            ),
            Shown::Email => (
                step::EMAIL,
                step::EMAIL_AGAIN,
                vec![input(field::EMAIL, "E-mail address", InputKind::Text, None)],
            ),
            Shown::EmailCode => (
                step::EMAIL_CODE,
                step::EMAIL_CODE_AGAIN,
                vec![input(
                    field::CODE,
                    "Code (sent by e-mail)",
                    InputKind::Text,
                    None,
                )],
            ),
            Shown::Nothing | Shown::Qr | Shown::Linking | Shown::Done => return None,
        };
        let id = if again { again_id } else { id };
        Some(self.step(id, LoginStepKind::Input { fields }))
    }

    /// A step with the next `txn`. A held INPUT request belonged to an older step: it goes.
    fn step(&mut self, id: &'static str, kind: LoginStepKind) -> Out {
        self.txn += 1;
        if self.queued.as_ref().is_some_and(|q| q.kind.is_input()) {
            self.queued = None;
        }
        Out::Step(LoginStep {
            step_id: id.to_owned(),
            txn: self.txn,
            kind,
        })
    }

    /// The terminal `Failed` step at `id`.
    fn failed(&mut self, id: &'static str, error: BackendError) -> Vec<Out> {
        let out = self.step(id, LoginStepKind::Failed { error });
        self.ended = true;
        self.queued = None;
        vec![out]
    }

    /// A refused identity after Ready: the `Failed` step, then the logout the session owes.
    fn refused(&mut self, refusal: Refusal) -> Vec<Out> {
        let id = self.flow.first_step();
        let mut outs = self.failed(id, refusal.error());
        outs.push(Out::Refused(refusal));
        outs
    }

    /// Sends `req` now, or holds it until the awaited answer came (the last one held wins).
    fn issue(&mut self, req: Request) -> Option<Out> {
        if self.pending.is_some() {
            self.queued = Some(req);
            None
        } else {
            self.pending = Some(req.kind);
            Some(Out::Request(req))
        }
    }
}

/// Whether `e`, the answer to a resend, is Telegram refusing THIS resend (J2). Not a flood, an
/// expired code, a banned number, an application that must update or an e-mail address that is not
/// allowed (all terminal, B1: they end the login as the same answer to any other request does), a
/// request in the wrong state (the state is read again) or the helper's end (a restart).
fn is_refused_resend(e: &LinkError) -> bool {
    match e {
        LinkError::Auth { class, .. } => !matches!(
            class,
            TdClass::TooManyRequests
                | TdClass::PhoneNumberFlood
                | TdClass::PhoneCodeExpired
                | TdClass::PhoneNumberBanned
                | TdClass::UpdateNeeded
                | TdClass::EmailNotAllowed
                | TdClass::AnotherQuery
                | TdClass::Unexpected
                | TdClass::FrozenMethodInvalid
        ),
        LinkError::Frame | LinkError::Refused(_) | LinkError::Td { .. } => true,
        LinkError::Timeout
        | LinkError::Closed
        | LinkError::HelloTimeout
        | LinkError::DatabaseLocked
        | LinkError::VersionMismatch => false,
    }
}

/// The detail of the QR bound and of a second WaitPhoneNumber in the QR flow (M6).
const QR_ENDED: &str = "the QR login ended without a scan";

/// How many typed values [`wipe`] was given (tests count the drop paths; a process of its own per
/// test under nextest, so a test asserts a rise of at least the values it dropped).
#[cfg(test)]
static WIPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The count of [`WIPED`].
#[cfg(test)]
pub(crate) fn wiped() -> usize {
    WIPED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Drops typed values the backend did not use: each one is moved into a `Zeroizing` first, so
/// it is zeroed when it goes (J14). EVERY path that drops a `SubmitLoginInput`'s values calls it.
pub(crate) fn wipe(values: Vec<(String, Redacted<String>)>) {
    #[cfg(test)]
    WIPED.fetch_add(values.len(), std::sync::atomic::Ordering::SeqCst);
    for (_, value) in values {
        drop(Zeroizing::new(value.into_inner()));
    }
}

/// The value of field `id`, MOVED out of its `Redacted` (M10); `None` when it is missing or empty.
fn take(values: &mut Vec<(String, Redacted<String>)>, id: &str) -> Option<Zeroizing<String>> {
    let at = values.iter().position(|(k, _)| k == id)?;
    let value = Zeroizing::new(values.swap_remove(at).1.into_inner());
    (!value.is_empty()).then_some(value)
}

fn input(id: &str, label: &str, kind: InputKind, note: Option<NetworkNote>) -> InputField {
    InputField {
        id: id.to_owned(),
        label: label.to_owned(),
        kind,
        default: None,
        required: true,
        note,
        code: None,
    }
}

/// The longest text of Telegram's own that Kourier keeps (the hint, a name), in characters: the
/// rule of WhatsApp's `translate::explanation` (J13).
const TEXT_MAX: usize = 400;

/// Foreign text as one line of plain text (J13, the rule of WhatsApp's `translate::explanation`):
/// the bidirectional controls that can reorder what follows are dropped, control characters and
/// runs of white space become one space, the whole is cut at [`TEXT_MAX`] characters and trimmed.
/// Empty gives `None`. Never markup, never logged.
pub(crate) fn plain_line(text: &str) -> Option<String> {
    let cleaned: String = text.chars().filter(|c| !is_bidi_control(*c)).collect();
    let one = cleaned
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = one.chars().take(TEXT_MAX).collect();
    let cut = cut.trim_end();
    (!cut.is_empty()).then(|| cut.to_owned())
}

/// Telegram's 2FA hint as the field's note (M1): [`plain_line`], `None` when empty.
fn note_of(hint: &str) -> Option<NetworkNote> {
    plain_line(hint).map(|one| NetworkNote {
        explanation: Some(one),
        appeal_url: None,
    })
}

fn request<T: Serialize>(kind: ReqKind, value: &T) -> Request {
    Request {
        kind,
        json: secret_json(value),
    }
}

// The requests, as borrowed structs (M10: no `serde_json::Value` holds a typed value).

/// A request (or a nested object) that is its `@type` alone.
#[derive(Serialize)]
struct Typed {
    #[serde(rename = "@type")]
    ty: &'static str,
}

#[derive(Serialize)]
struct RequestQr<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    other_user_ids: &'a [i64],
}

#[derive(Serialize)]
struct SetPhone<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    phone_number: &'a str,
    settings: PhoneSettings,
}

/// `phoneNumberAuthenticationSettings` (`td_api.tl:7642`): everything off, no Firebase ("for
/// official Android and iOS applications only"), no tokens of an earlier logout (L4: a relink then
/// always needs a new code).
#[derive(Serialize)]
struct PhoneSettings {
    #[serde(rename = "@type")]
    ty: &'static str,
    allow_flash_call: bool,
    allow_missed_call: bool,
    is_current_phone_number: bool,
    has_unknown_phone_number: bool,
    allow_sms_retriever_api: bool,
    firebase_authentication_settings: Option<()>,
    authentication_tokens: [&'static str; 0],
}

impl PhoneSettings {
    const NONE: Self = Self {
        ty: "phoneNumberAuthenticationSettings",
        allow_flash_call: false,
        allow_missed_call: false,
        is_current_phone_number: false,
        has_unknown_phone_number: false,
        allow_sms_retriever_api: false,
        firebase_authentication_settings: None,
        authentication_tokens: [],
    };
}

#[derive(Serialize)]
struct Resend {
    #[serde(rename = "@type")]
    ty: &'static str,
    reason: Typed,
}

#[derive(Serialize)]
struct CheckCode<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    code: &'a str,
}

#[derive(Serialize)]
struct CheckPassword<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    password: &'a str,
}

#[derive(Serialize)]
struct SetEmail<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    email_address: &'a str,
}

#[derive(Serialize)]
struct CheckEmailCode<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    code: EmailCode<'a>,
}

/// `emailAddressAuthenticationCode` (`td_api.tl:89`).
#[derive(Serialize)]
struct EmailCode<'a> {
    #[serde(rename = "@type")]
    ty: &'static str,
    code: &'a str,
}

#[cfg(test)]
mod tests;
