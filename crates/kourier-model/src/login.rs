//! Login steps a backend streams while linking (architecture §4.4).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::conn::NetworkNote;
use crate::error::BackendError;
use crate::log::Redacted;

/// One step of a login flow. `txn` identifies the step instance that `SubmitLoginInput` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginStep {
    /// Stable step name ("device-name", "qr", "linking", …). The link page's words are keyed on
    /// it: the shared vocabulary is `qr`, `phone`, `code`, `password` (a two-step verification
    /// password), `email` (a login e-mail address the network asks for), `email-code` (the code
    /// sent to that address), `passkey` and `done`, and a failed step carries the step it failed
    /// at (a number the network refused fails at `phone`, a code that ran out at `code`); a step
    /// it does not know gets the generic hint. A `Failed` step ENDS the login (the core discards
    /// the account), so an input the network refused but lets the person type again (a wrong code
    /// or password) is not `Failed`: the same input step comes again, with a new `txn`, under the
    /// retry id `code-again`, `password-again`, `email-again` or `email-code-again`, whose words
    /// say that the last entry was not right.
    ///
    /// Three more ids exist only as the step a `Failed` login failed at (a FAILURE-only id never
    /// asks for input; the words are the UI's own, from the step and the error kind, never from
    /// the error's detail): `registration` (the number has no account: the network wants one
    /// created first), `purchase` (the network asks for a purchase before it lets the account
    /// log in) and `update` (the network asks for a newer app). They name the step the network
    /// asked for and Kourier does not do; Telegram is their only sender (KT-1b, LC-KT1b-2).
    ///
    /// The value id [`RESEND_FIELD`] of `SubmitLoginInput` asks the network for a new login code
    /// (any value; no other value is sent with it).
    pub step_id: String,
    /// Transaction number; increases with every step that expects input.
    pub txn: u64,
    /// What to show.
    pub kind: LoginStepKind,
}

/// What the generic login page shows for a step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LoginStepKind {
    /// A QR code; may repeat (rotation). The contents are secret.
    ShowQr {
        /// QR payload.
        data: Redacted<String>,
        /// How long it stays valid.
        #[serde(default)]
        valid_for: Option<Duration>,
    },
    /// A pairing code to type on the phone (WhatsApp).
    ShowCode {
        /// The code.
        code: Redacted<String>,
    },
    /// Emoji to compare (Threema rendezvous path hash).
    ConfirmEmoji {
        /// The emoji.
        emoji: Vec<String>,
    },
    /// Fields to fill in; answered with `SubmitLoginInput`.
    Input {
        /// The fields.
        fields: Vec<InputField>,
    },
    /// Work in progress.
    Progress {
        /// User-facing label.
        label: String,
        /// 0.0 … 1.0, when known.
        #[serde(default)]
        fraction: Option<f32>,
    },
    /// Login finished.
    Done,
    /// Login failed.
    Failed {
        /// Why.
        error: BackendError,
    },
}

/// One input field of a [`LoginStepKind::Input`] step. `Debug` hides the prefilled value
/// (the hostname may name a person) and the network's note.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputField {
    /// Key used in `SubmitLoginInput.values`.
    pub id: String,
    /// User-facing label.
    pub label: String,
    /// Field type.
    #[serde(default)]
    pub kind: InputKind,
    /// Prefilled value (device name: the hostname).
    #[serde(default)]
    pub default: Option<String>,
    /// Must not be empty.
    #[serde(default)]
    pub required: bool,
    /// What the network itself said about this field (Telegram's hint of a two-step
    /// verification password, the account owner's own words): foreign text under the rules of
    /// [`NetworkNote`] (plain text, one line, never logged), shown beside the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<NetworkNote>,
    /// How the network sent the login code this field asks for, and whether a new code may be
    /// asked for (TSC M2, KT-1b): typed, so the page's words are the UI's own and no backend
    /// text. Set on the code field of a step only (Telegram's `code`, never `email-code`); every
    /// other field has `None`. Not secret: it names a way, never the code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<CodeDelivery>,
}

crate::redacting_debug!(InputField { id, label, kind, required, code } redact { default, note });

/// The value id of `SubmitLoginInput` that asks the network for a new login code (any value;
/// it is sent alone, with no other value). One definition: the UI sends it and a backend reads
/// it (Telegram's `resend`, KT-1b).
pub const RESEND_FIELD: &str = "resend";

/// How a network sent a login code, and whether the person may ask for another one (TSC M2,
/// KT-1b). Typed, so the UI writes its own words for it and no sentence of the network's
/// reaches the page. A network that sends no code says nothing (`InputField::code` is `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeDelivery {
    /// Where the code went.
    pub via: CodeVia,
    /// Another code may be asked for (`SubmitLoginInput` with [`RESEND_FIELD`]): `None` = not
    /// now and not by this way (the network offers no next way, or the way is one Kourier
    /// cannot take).
    #[serde(default)]
    pub resend: Option<CodeResend>,
}

/// Where a login code was sent. Unit variants, so a serde sample is one name each; the first
/// letter or word of an SMS word or phrase is the field's `note`, never a variant's payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CodeVia {
    /// A message in the network's own app on another device.
    App,
    /// A text message.
    Sms,
    /// A text message with a word to type (the first letter is the field's note).
    SmsWord,
    /// A text message with a phrase to type (the first word is the field's note).
    SmsPhrase,
    /// A phone call that says the code.
    Call,
    /// The network's marketplace for anonymous numbers (Telegram's Fragment).
    Fragment,
    /// A way this vocabulary has no word for.
    Other,
}

/// A new code the person may ask for, and by which way it would come.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeResend {
    /// The wait that REMAINS when the step is shown, not the network's whole timeout: a step
    /// shown again (a wrong entry, an early ask) states what is left, so a page that restarts
    /// its timer at every step is never late. Zero: the new code may be asked for at once.
    pub after: Duration,
    /// Where the new code would be sent.
    pub via: CodeVia,
}

/// Type of an [`InputField`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InputKind {
    /// Free text.
    #[default]
    Text,
    /// Hidden input (password).
    Secret,
    /// Phone number.
    Phone,
}
