//! The generic login page (§4.4, §8.3): a [`LoginStep`] as the fields QML renders, and the
//! networks the welcome page offers.

use kourier_model::{BackendError, InputKind, LoginStep, LoginStepKind, NetworkKind};

use crate::conversations::{network_icon, network_name};
use crate::i18n::{trc, trfc};
use crate::names::single_line;

/// What the login page shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LoginViewKind {
    /// Nothing yet (waiting for the first step).
    #[default]
    Waiting,
    /// A QR code to scan.
    Qr,
    /// A pairing code to type on the phone.
    Code,
    /// Emoji to compare.
    Emoji,
    /// Input fields.
    Input,
    /// Work in progress.
    Progress,
    /// Linked.
    Done,
    /// Failed; may be retried.
    Failed,
}

impl LoginViewKind {
    /// Stable name for QML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Qr => "qr",
            Self::Code => "code",
            Self::Emoji => "emoji",
            Self::Input => "input",
            Self::Progress => "progress",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// One input field.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct FieldView {
    /// Key for the submitted values.
    pub id: String,
    /// Label.
    pub label: String,
    /// `text`, `secret` or `phone`.
    pub kind: String,
    /// Prefilled value.
    pub value: String,
    /// Must not be empty.
    pub required: bool,
}

crate::safe_debug!(FieldView { id, kind, required });

/// A login step as the page renders it. `qr` and `code` are secrets: never log them.
#[derive(Clone, Default, PartialEq)]
pub struct LoginView {
    /// The step being shown.
    pub step_id: String,
    /// Its transaction number.
    pub txn: u64,
    /// What to render.
    pub kind: LoginViewKind,
    /// Heading.
    pub title: String,
    /// Instructions below the heading.
    pub hint: String,
    /// QR payload (`sgnl://linkdevice?…`).
    pub qr: String,
    /// Pairing code.
    pub code: String,
    /// Emoji to compare.
    pub emoji: Vec<String>,
    /// Input fields.
    pub fields: Vec<FieldView>,
    /// Progress 0 … 1, or `-1` when indeterminate.
    pub progress: f64,
    /// Error text of a failed step.
    pub error: String,
    /// "Try again" is offered.
    pub retry: bool,
    /// The label of the button that leaves a failed link for the conversation list (empty:
    /// none). A link that cannot be tried again, because the account is in Kourier already,
    /// offers it in place of "Try Again" (user review of L1).
    pub leave: String,
}

impl std::fmt::Debug for LoginView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginView")
            .field("step_id", &self.step_id)
            .field("txn", &self.txn)
            .field("kind", &self.kind)
            .field("qr", &"[redacted]")
            .field("code", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// Builds the view of `step` for a login to `network`.
pub fn login_view(step: &LoginStep, network: NetworkKind) -> LoginView {
    let net = network_name(network);
    let mut v = LoginView {
        step_id: step.step_id.clone(),
        txn: step.txn,
        progress: -1.0,
        ..LoginView::default()
    };
    match &step.kind {
        LoginStepKind::ShowQr { data, .. } => {
            v.kind = LoginViewKind::Qr;
            v.title = link_title(net);
            v.hint = match network {
                NetworkKind::Signal => trc(
                    "@info:usage how to link a Signal account",
                    "On your phone, open Signal → Settings → Linked devices → Link new device, and scan this code.",
                )
                .into_owned(),
                _ => trc(
                    "@info:usage how to link an account",
                    "Scan this code with the app on your phone.",
                )
                .into_owned(),
            };
            v.qr = data.expose().clone();
        }
        LoginStepKind::ShowCode { code } => {
            v.kind = LoginViewKind::Code;
            v.title = link_title(net);
            v.hint = trc(
                "@info:usage how to link an account",
                "Enter this code on your phone.",
            )
            .into_owned();
            v.code = code.expose().clone();
        }
        LoginStepKind::ConfirmEmoji { emoji } => {
            v.kind = LoginViewKind::Emoji;
            v.title =
                trc("@title:window page to link an account", "Compare the Emoji").into_owned();
            v.hint = trc(
                "@info:usage how to link an account",
                "Confirm that your phone shows the same emoji.",
            )
            .into_owned();
            v.emoji = emoji.clone();
        }
        LoginStepKind::Input { fields } => {
            v.kind = LoginViewKind::Input;
            v.title = link_title(net);
            v.fields = fields
                .iter()
                .map(|f| FieldView {
                    id: f.id.clone(),
                    label: single_line(&f.label),
                    kind: match f.kind {
                        InputKind::Text => "text",
                        InputKind::Secret => "secret",
                        InputKind::Phone => "phone",
                    }
                    .into(),
                    value: f.default.clone().unwrap_or_default(),
                    required: f.required,
                })
                .collect();
        }
        LoginStepKind::Progress { label, fraction } => {
            v.kind = LoginViewKind::Progress;
            v.title = trc("@title:window page to link an account", "Linking…").into_owned();
            v.hint = single_line(label);
            v.progress = fraction.map_or(-1.0, |f| f64::from(f.clamp(0.0, 1.0)));
        }
        LoginStepKind::Done => {
            v.kind = LoginViewKind::Done;
            v.title = trc("@title:window page to link an account", "Linked").into_owned();
            v.progress = 1.0;
        }
        LoginStepKind::Failed { error } => {
            v.kind = LoginViewKind::Failed;
            v.title = trc("@title:window", "Linking Failed").into_owned();
            v.error = error_text(error);
            // A second link of an account that is here already would only fail again: no
            // "Try Again". An active account is looked at in the list; a logged-out one is
            // linked again from its banner, which is on the list too.
            match &error.kind {
                kourier_model::ErrorKind::AlreadyAdded { logged_out: false } => {
                    v.retry = false;
                    v.leave = trc("@action:button close a message and go to the chats", "OK")
                        .into_owned();
                }
                kourier_model::ErrorKind::AlreadyAdded { logged_out: true } => {
                    v.retry = false;
                    v.leave = trc(
                        "@action:button go to the chats, where the banner of the account that must be linked again is",
                        "Go to Chats",
                    )
                    .into_owned();
                }
                _ => v.retry = true,
            }
        }
    }
    v
}

/// The title of the page that links an account of the network called `net`.
fn link_title(net: &str) -> String {
    trfc(
        "@title:window page to link an account, %1 the name of a messenger",
        "Link %1",
        &[net],
    )
}

/// User-facing text of a login error.
pub fn error_text(error: &BackendError) -> String {
    use kourier_model::ErrorKind;
    match &error.kind {
        ErrorKind::Transient { .. } => trc(
            "@info:status why linking failed",
            "Could not reach the server. Check your connection.",
        ),
        ErrorKind::RateLimited { .. } => trc(
            "@info:status why linking failed",
            "Too many attempts. Wait a moment and try again.",
        ),
        ErrorKind::Expired => trc(
            "@info:status why linking failed",
            "The code expired. Try again for a new one.",
        ),
        ErrorKind::AlreadyAdded { logged_out: false } => trc(
            "@info:status why linking failed",
            "This account is already in Kourier.",
        ),
        ErrorKind::AlreadyAdded { logged_out: true } => trc(
            "@info:status why linking failed",
            "This account is already in Kourier. To use it again, choose “Link Again” in its banner.",
        ),
        ErrorKind::Unauthorized | ErrorKind::LoggedOut => trc(
            "@info:status why linking failed",
            "The phone rejected the link.",
        ),
        _ => trc("@info:status why linking failed", "Something went wrong."),
    }
    .into_owned()
}

/// A network on the welcome page.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NetworkChoice {
    /// The network.
    pub kind: NetworkKind,
    /// Its name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Badge icon key.
    pub icon: String,
    /// Can be linked in this build.
    pub available: bool,
    /// The network hands a newly linked device the account's history (its descriptor says
    /// `HistorySource::PushedOnLink`): an account of it offers "Transfer History from Phone…".
    pub history_on_link: bool,
}

/// The networks offered on the welcome page (M1: Signal only; the others are announced).
pub fn network_choices() -> Vec<NetworkChoice> {
    let choice = |kind, description: String, available| NetworkChoice {
        kind,
        name: network_name(kind).to_owned(),
        description,
        icon: network_icon(kind).to_owned(),
        available,
        // What the build's networks say is filled in by the front-end's composition.
        history_on_link: false,
    };
    let coming_later = || {
        trc(
            "@info:status a messenger that cannot be linked yet",
            "Coming later",
        )
        .into_owned()
    };
    vec![
        choice(
            NetworkKind::Signal,
            trc(
                "@info a messenger that can be linked",
                "Link as a secondary device of the Signal app on your phone",
            )
            .into_owned(),
            true,
        ),
        choice(NetworkKind::Threema, coming_later(), false),
        choice(NetworkKind::WhatsApp, coming_later(), false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::{ErrorKind, Redacted};

    #[test]
    fn qr_step_and_redaction() {
        let step = LoginStep {
            step_id: "qr".into(),
            txn: 3,
            kind: LoginStepKind::ShowQr {
                data: Redacted::new("sgnl://linkdevice?uuid=secret".to_owned()),
                valid_for: None,
            },
        };
        let v = login_view(&step, NetworkKind::Signal);
        assert_eq!(v.kind, LoginViewKind::Qr);
        assert_eq!(v.qr, "sgnl://linkdevice?uuid=secret");
        assert_eq!(v.title, "Link Signal");
        assert!(!format!("{v:?}").contains("secret"));
    }

    #[test]
    fn progress_and_failure() {
        let step = LoginStep {
            step_id: "sync".into(),
            txn: 4,
            kind: LoginStepKind::Progress {
                label: "Syncing contacts".into(),
                fraction: Some(1.7),
            },
        };
        assert_eq!(login_view(&step, NetworkKind::Signal).progress, 1.0);
        let step = LoginStep {
            step_id: "qr".into(),
            txn: 5,
            kind: LoginStepKind::Failed {
                error: BackendError::new(ErrorKind::Expired, "qr expired"),
            },
        };
        let v = login_view(&step, NetworkKind::Signal);
        assert!(v.retry);
        assert_eq!(v.error, "The code expired. Try again for a new one.");
        assert_eq!(network_choices().iter().filter(|c| c.available).count(), 1);
    }

    /// A second link of an account the profile has says so (L1, `buttons.md` F2), and points
    /// at "Link Again" when that account is logged out.
    #[test]
    fn a_link_of_an_account_already_here_says_so() {
        let failed = |logged_out| LoginStep {
            step_id: "already-added".into(),
            txn: 0,
            kind: LoginStepKind::Failed {
                error: BackendError::new(ErrorKind::AlreadyAdded { logged_out }, "dup"),
            },
        };
        let v = login_view(&failed(false), NetworkKind::Signal);
        assert_eq!(v.kind, LoginViewKind::Failed);
        assert_eq!(v.error, "This account is already in Kourier.");
        let v = login_view(&failed(true), NetworkKind::Signal);
        assert!(v.error.contains("“Link Again”"), "{}", v.error);
    }

    /// A second link of an account that is here would only fail again: no "Try Again". The
    /// page offers the way on instead: an OK to the list for an active account, "Go to Chats"
    /// for a logged-out one (its banner with "Link Again" is on the list; L5 renamed the
    /// button, which said "Show Banner").
    #[test]
    fn a_link_of_an_account_already_here_offers_no_retry_but_the_way_back() {
        let failed = |kind| LoginStep {
            step_id: "failed".into(),
            txn: 0,
            kind: LoginStepKind::Failed {
                error: BackendError::new(kind, "dup"),
            },
        };
        let active = login_view(
            &failed(ErrorKind::AlreadyAdded { logged_out: false }),
            NetworkKind::Signal,
        );
        assert!(!active.retry, "no Try Again for an active account");
        assert_eq!(active.leave, "OK");
        let out = login_view(
            &failed(ErrorKind::AlreadyAdded { logged_out: true }),
            NetworkKind::Signal,
        );
        assert!(!out.retry, "no Try Again for a logged-out account");
        assert_eq!(out.leave, "Go to Chats");
        // Every other failure still can be tried again and offers no other way.
        for kind in [ErrorKind::Expired, ErrorKind::Unauthorized] {
            let v = login_view(&failed(kind), NetworkKind::Signal);
            assert!(v.retry && v.leave.is_empty(), "{:?}", v.error);
        }
    }
}
