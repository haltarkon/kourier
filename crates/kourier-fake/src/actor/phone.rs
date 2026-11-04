//! The `phone` login flow of a network that sends a login code (Telegram, KT-1b), and the
//! password step a QR scan is followed by: the fake's end of TDLib's authorization states, in
//! TDLib's order. A child module of the actor so that it reads the actor's private state.
//!
//! The steps of the phone flow, each with the next `txn` (the number's step is `txn` 1, so a
//! failure at it carries 2, like the `pair-code` flow's):
//!
//! 1. `phone`: the number, typed. Not a number: `Failed` at `phone`, `InvalidInput`. A number
//!    that misses the tail the run tool expects ([`Script::honour_number_tail`]): `Failed` at
//!    `phone`, `NotTheExpectedAccount`, BEFORE any code (the real order: the backend checks the
//!    typed number before it asks Telegram to send one). A number in no row of the table: `Failed`
//!    at `registration`, `Unsupported` (Telegram's "no account", the step Kourier does not do).
//!    A row with a refusal: `Failed` at `phone` with that error.
//! 2. `code`: one field `code`, whose [`CodeDelivery`] says how the code was sent and what is left
//!    of the wait before a new one (the REMAINING wait at every showing, on the account clock). A
//!    wrong code shows `code-again`; a [`RESEND_FIELD`] value before the wait is over shows `code`
//!    again with the wait that is left, after it a new code: `code` again with the resend's way and
//!    no further resend. An answer with neither value shows `code` again.
//! 3. `password`, when the row has one: the field's note is the owner's hint; wrong: `password-again`.
//! 4. `email`, `email-code`, when the row has an e-mail code: an address without `@` shows
//!    `email-again`; a wrong code `email-code-again`.
//! 5. The link (`linking`, `AccountReady`, `done`).
//!
//! A cancel ends the login at every wait; an input whose `txn` is not the last step's is stale and
//! ignored (the real machine's rule, L7).

use std::time::Duration;

use kourier_backend_api::Relink;
use kourier_model::{
    BackendError, CodeDelivery, CodeResend, ErrorKind, InputField, InputKind, LoginStepKind,
    NetworkNote, RESEND_FIELD, Redacted,
};
use tokio::sync::mpsc;

use super::{Actor, LoginCmd};
use crate::script::{PasswordScript, PhoneAccount, PhoneLoginScript, digits_of};

/// The values of one `SubmitLoginInput`.
type Values = Vec<(String, Redacted<String>)>;

/// The non-empty value typed into field `id`.
fn typed<'a>(values: &'a Values, id: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|(k, _)| k == id)
        .map(|(_, v)| v.expose().as_str())
        .filter(|v| !v.is_empty())
}

/// Waits for the answer to step `txn`: its values, or `None` when the login is cancelled (or the
/// core is gone). Another step's answer and any other command are ignored.
async fn answer(rx: &mut mpsc::UnboundedReceiver<LoginCmd>, txn: u64) -> Option<Values> {
    loop {
        match rx.recv().await {
            Some(LoginCmd::Input(t, values)) if t == txn => return Some(values),
            Some(LoginCmd::Cancel) | None => return None,
            Some(_) => continue,
        }
    }
}

/// An input field of the phone flow (no default, always required).
fn field(id: &str, label: &str, kind: InputKind) -> InputField {
    InputField {
        id: id.into(),
        label: label.into(),
        kind,
        default: None,
        required: true,
        note: None,
        code: None,
    }
}

/// The digits of a typed phone number: `+` and the separators the real backend drops are accepted,
/// any other character makes it no number; at least one digit.
fn number_digits(typed: &str) -> Option<String> {
    let rest = typed
        .trim()
        .strip_prefix('+')
        .unwrap_or_else(|| typed.trim());
    let plain = rest
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '-' | '.' | '(' | ')'));
    let digits = digits_of(rest);
    (plain && !digits.is_empty()).then_some(digits)
}

/// The word of `expected_number_check` in the login-start line: `on` when the run tool expects a
/// tail, `off` when it does not; never the tail.
pub(super) fn expected_number_check(tail_set: bool) -> &'static str {
    if tail_set { "on" } else { "off" }
}

/// What the code step states at the moment `now_ms`: the way the code came (after a new code was
/// asked for, the resend's way) and, until that new code, the wait that REMAINS of the account's
/// whole wait (it started when the first code step was shown, at `shown_ms`). A new code asked for
/// is the last: no further resend.
fn code_delivery(account: &PhoneAccount, shown_ms: u64, now_ms: u64, resent: bool) -> CodeDelivery {
    let delivery = account.delivery;
    match delivery.resend {
        None => delivery,
        Some(resend) if resent => CodeDelivery {
            via: resend.via,
            resend: None,
        },
        Some(resend) => CodeDelivery {
            via: delivery.via,
            resend: Some(CodeResend {
                after: Duration::from_millis(due_ms(shown_ms, resend).saturating_sub(now_ms)),
                via: resend.via,
            }),
        },
    }
}

/// When a new code may be asked for, on the account clock.
fn due_ms(shown_ms: u64, resend: CodeResend) -> u64 {
    shown_ms.saturating_add(u64::try_from(resend.after.as_millis()).unwrap_or(u64::MAX))
}

impl Actor {
    /// Emits the next input step (its `txn` is `*txn + 1`) and waits for its answer.
    async fn ask(
        &self,
        rx: &mut mpsc::UnboundedReceiver<LoginCmd>,
        txn: &mut u64,
        step_id: &str,
        field: InputField,
    ) -> Option<Values> {
        *txn += 1;
        self.step_login(
            step_id,
            *txn,
            LoginStepKind::Input {
                fields: vec![field],
            },
        )
        .await;
        answer(rx, *txn).await
    }

    /// Ends the login `Failed` at `step_id` (the step after the last one shown).
    async fn fail_at(
        &self,
        txn: u64,
        step_id: &str,
        error: BackendError,
    ) -> Result<bool, BackendError> {
        self.step_login(step_id, txn + 1, LoginStepKind::Failed { error })
            .await;
        Ok(false)
    }

    /// The `phone` flow ([`PhoneLoginScript`]): the number, the code, the password, the e-mail
    /// steps, the link. `Ok(false)` when it did not link.
    pub(super) async fn phone_login(
        &self,
        phone: &PhoneLoginScript,
        relink: Option<&Relink>,
        rx: &mut mpsc::UnboundedReceiver<LoginCmd>,
    ) -> Result<bool, BackendError> {
        let mut txn = 0;
        // The number.
        let Some(values) = self
            .ask(
                rx,
                &mut txn,
                "phone",
                field("phone", "Phone number", InputKind::Phone),
            )
            .await
        else {
            return Ok(false);
        };
        let Some(digits) = typed(&values, "phone").and_then(number_digits) else {
            let error = BackendError::new(
                ErrorKind::InvalidInput,
                "the typed text is not a phone number",
            );
            return self.fail_at(txn, "phone", error).await;
        };
        if self.tail_refuses(&digits) {
            let error = BackendError::new(
                ErrorKind::NotTheExpectedAccount,
                "the number is not the expected one",
            );
            return self.fail_at(txn, "phone", error).await;
        }
        let Some(account) = phone.account(&digits) else {
            let error = BackendError::new(ErrorKind::Unsupported, "this number has no account");
            return self.fail_at(txn, "registration", error).await;
        };
        if let Some(error) = &account.refusal {
            return self.fail_at(txn, "phone", error.clone()).await;
        }

        // The code, and a new one when it is asked for and allowed.
        let shown_ms = self.clock.now_ms();
        let mut resent = false;
        let mut step_id = "code";
        loop {
            let delivery = code_delivery(account, shown_ms, self.clock.now_ms(), resent);
            let code = InputField {
                code: Some(delivery),
                ..field("code", "Code", InputKind::Text)
            };
            let Some(values) = self.ask(rx, &mut txn, step_id, code).await else {
                return Ok(false);
            };
            step_id = "code";
            if typed(&values, RESEND_FIELD).is_some() {
                let allowed = !resent
                    && account
                        .delivery
                        .resend
                        .is_some_and(|r| self.clock.now_ms() >= due_ms(shown_ms, r));
                resent |= allowed;
            } else if let Some(code) = typed(&values, "code") {
                if code == account.code {
                    break;
                }
                step_id = "code-again";
            }
        }

        if let Some(password) = &account.password
            && !self.password_steps(rx, &mut txn, password).await
        {
            return Ok(false);
        }
        if let Some(email_code) = &account.email_code
            && !self.email_steps(rx, &mut txn, email_code).await
        {
            return Ok(false);
        }
        self.link(relink, &self.script.own_ids).await
    }

    /// The account the phone scans in after a QR code is the script's own number: the password
    /// of its row, if it has one (TDLib's `WaitPassword` after a scan).
    pub(super) fn qr_password(&self) -> Option<&PasswordScript> {
        let number = digits_of(self.script.number.as_deref()?);
        self.script
            .phone_login
            .as_ref()?
            .account(&number)?
            .password
            .as_ref()
    }

    /// The `password` step and its retries; `false` when the login was cancelled.
    pub(super) async fn password_steps(
        &self,
        rx: &mut mpsc::UnboundedReceiver<LoginCmd>,
        txn: &mut u64,
        password: &PasswordScript,
    ) -> bool {
        let mut step_id = "password";
        loop {
            let field = InputField {
                note: password.hint.clone().map(|hint| NetworkNote {
                    explanation: Some(hint),
                    appeal_url: None,
                }),
                ..field("password", "Password", InputKind::Secret)
            };
            let Some(values) = self.ask(rx, txn, step_id, field).await else {
                return false;
            };
            step_id = "password";
            match typed(&values, "password") {
                Some(typed) if typed == password.password => return true,
                Some(_) => step_id = "password-again",
                None => {}
            }
        }
    }

    /// The `email` and `email-code` steps and their retries; `false` when the login was cancelled.
    async fn email_steps(
        &self,
        rx: &mut mpsc::UnboundedReceiver<LoginCmd>,
        txn: &mut u64,
        email_code: &str,
    ) -> bool {
        let mut step_id = "email";
        loop {
            let Some(values) = self
                .ask(
                    rx,
                    txn,
                    step_id,
                    field("email", "Email address", InputKind::Text),
                )
                .await
            else {
                return false;
            };
            step_id = "email";
            match typed(&values, "email") {
                Some(address) if address.contains('@') => break,
                Some(_) => step_id = "email-again",
                None => {}
            }
        }
        let mut step_id = "email-code";
        loop {
            let Some(values) = self
                .ask(rx, txn, step_id, field("code", "Code", InputKind::Text))
                .await
            else {
                return false;
            };
            step_id = "email-code";
            match typed(&values, "code") {
                Some(typed) if typed == email_code => return true,
                Some(_) => step_id = "email-code-again",
                None => {}
            }
        }
    }

    /// Whether the TYPED `number` misses the tail the run tool expects: the script honours the
    /// tail, one is set, and the number does not end in it (an empty tail fails closed, as the
    /// real backend's does).
    pub(super) fn tail_refuses(&self, number: &str) -> bool {
        if !self.script.honour_number_tail {
            return false;
        }
        let Some(tail) = &self.number_tail else {
            return false;
        };
        let tail = digits_of(tail.expose());
        tail.is_empty() || !digits_of(number).ends_with(&tail)
    }

    /// The line a real login prints when it starts, for a script that honours the number tail
    /// (the rehearsals'): the run tool says "scan now" only after it has read this token (L2). The
    /// word of `expected_number_check` is `on` or `off`, never the tail. The fake that plays
    /// Telegram prints Telegram's words, every other fake WhatsApp's (the words of the real
    /// backends, fixed once; `tests/telegram.rs` pins this source).
    pub(super) fn log_login_start(&self) {
        if !self.script.honour_number_tail {
            return;
        }
        let check = expected_number_check(self.number_tail.is_some());
        match self.kind {
            kourier_model::NetworkKind::Telegram => {
                tracing::info!(expected_number_check = check, "a telegram login starts");
            }
            _ => tracing::info!(expected_number_check = check, "a whatsapp login starts"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_start_line_says_on_when_a_tail_is_expected_and_off_when_it_is_not() {
        assert_eq!(expected_number_check(true), "on");
        assert_eq!(expected_number_check(false), "off");
    }

    #[test]
    fn a_typed_number_is_digits_with_the_separators_the_real_backend_drops() {
        for (typed, want) in [
            ("+999 123-456.789", Some("999123456789")),
            (" 999123456789 ", Some("999123456789")),
            ("+(999) 123", Some("999123")),
            ("", None),
            ("+", None),
            ("999x123", None),
            ("not a number", None),
        ] {
            assert_eq!(number_digits(typed).as_deref(), want, "{typed:?}");
        }
    }
}
