//! Login steps a backend streams while linking (architecture §4.4).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::BackendError;
use crate::log::Redacted;

/// One step of a login flow. `txn` identifies the step instance that `SubmitLoginInput` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginStep {
    /// Stable step name ("device-name", "qr", "linking", …).
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
/// (the hostname may name a person).
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
}

crate::redacting_debug!(InputField { id, label, kind, required } redact { default });

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
