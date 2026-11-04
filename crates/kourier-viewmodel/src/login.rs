//! The generic login page (§4.4, §8.3): a [`LoginStep`] as the fields QML renders, and the
//! networks the welcome page offers.

use std::time::Duration;

use kourier_model::{
    BackendError, CodeVia, HistorySource, InputField, InputKind, LoginStep, LoginStepKind,
    NetworkKind,
};

use crate::conversations::{network_icon, network_name, note_line};
use crate::i18n::{trc, trfc, trnfc};
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
    /// The quiet line under the field (empty: none): Telegram's hint of a two-step verification
    /// password ("Hint: …") or the first letter or word of a code sent by SMS ("It starts with
    /// “…”"). The words around the network's text are the viewmodel's; the network's text is
    /// foreign: one cut line, shown as plain text, never logged (so `Debug` leaves it out).
    pub note: String,
}

crate::safe_debug!(FieldView { id, kind, required });

/// One emoji of the comparison step: the glyph the phone shows and its name (QB: short,
/// capitalised, one or two words; empty for a glyph the table does not know).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct EmojiView {
    /// The emoji as the backend sent it (code points, variation selectors included).
    pub glyph: String,
    /// Its name under the glyph and for the screen reader ([`emoji_name`]).
    pub name: String,
}

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
    /// The QR code is replaced by a new one while the page waits, and the page says so ("The code
    /// changes every few seconds"). False for Threema, whose code is one for the whole link, and
    /// for every step that shows no QR code.
    pub rotates: bool,
    /// Pairing code.
    pub code: String,
    /// Emoji to compare, each with its name.
    pub emoji: Vec<EmojiView>,
    /// Input fields.
    pub fields: Vec<FieldView>,
    /// Progress 0 … 1, or `-1` when indeterminate.
    pub progress: f64,
    /// Error text of a failed step.
    pub error: String,
    /// "Try again" is offered.
    pub retry: bool,
    /// The id of the flow that runs (`qr`, `pair-code`); empty until the source says.
    pub flow: String,
    /// The other flow of the network, which the page offers to switch to (empty: none), and the
    /// label of the button that does it ("Link with Phone Number Instead").
    pub other_flow: String,
    /// See [`LoginView::other_flow`].
    pub other_flow_label: String,
    /// Numbered instructions for the person (empty: the hint stands alone). The words are the
    /// viewmodel's, keyed on the network and the step's purpose (`step_id`), never the
    /// backend's.
    pub steps: Vec<String>,
    /// The line above input fields (empty: none).
    pub explain: String,
    /// The network's icon key (the page's badge and the input icon).
    pub icon: String,
    /// The label of the button that leaves a failed link for the conversation list (empty:
    /// none). A link that cannot be tried again, because the account is in Kourier already,
    /// offers it in place of "Try Again" (user review of L1).
    pub leave: String,
    /// The label of the button that asks the network for a new login code (empty: the network
    /// offers none now, so the page shows no button: an action that does nothing is not shown).
    /// A click sends [`kourier_model::RESEND_FIELD`] alone.
    pub resend: String,
    /// How long after the step is shown the button appears (ms): the wait that REMAINS, as the
    /// network states it at every showing of the step; 0 = at once. The page restarts its timer
    /// whenever a new input step is shown.
    pub resend_after_ms: u64,
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
        icon: network_icon(network).to_owned(),
        ..LoginView::default()
    };
    // The words are keyed on the network and the step's PURPOSE (`step_id`): a step of a
    // purpose the viewmodel does not know (a passkey's QR code, W9) gets the generic hint, not
    // another purpose's instructions.
    let purpose = step.step_id.as_str();
    match &step.kind {
        LoginStepKind::ShowQr { data, .. } => {
            v.kind = LoginViewKind::Qr;
            // Every QR code of this app is replaced by a new one while the page waits, except
            // Threema's: ONE code for the link (QD). Keyed on the network, not on `valid_for`:
            // Telegram's token rotates and says no time (`None`), and the fake that plays Threema
            // gives its code a time.
            v.rotates = network != NetworkKind::Threema;
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
            if purpose == "qr" {
                v.steps = qr_steps(network, net);
            }
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
            if purpose == "code" && network == NetworkKind::WhatsApp {
                v.steps = code_steps(net);
                v.hint = trc(
                    "@info:usage how long a code to type on the phone is valid",
                    "The code is valid for about three minutes. Your phone may also show a notification to enter it.",
                )
                .into_owned();
            }
            v.code = display_code(code.expose());
        }
        LoginStepKind::ConfirmEmoji { emoji } => {
            v.kind = LoginViewKind::Emoji;
            v.title =
                trc("@title:window page to link an account", "Compare the Emoji").into_owned();
            v.hint = trc(
                "@info:usage how to link an account",
                "Check that your phone shows the same three emoji, then confirm on your phone.",
            )
            .into_owned();
            v.emoji = emoji
                .iter()
                .map(|glyph| EmojiView {
                    glyph: glyph.clone(),
                    name: emoji_name(glyph).unwrap_or_default(),
                })
                .collect();
        }
        LoginStepKind::Input { fields } => {
            v.kind = LoginViewKind::Input;
            v.title = link_title(net);
            // How the network sent the code this step asks for (the code field alone states it).
            let delivery = fields.iter().find_map(|f| f.code);
            v.explain = input_explain(network, purpose, delivery.map(|d| d.via));
            v.fields = fields
                .iter()
                .map(|f| FieldView {
                    id: f.id.clone(),
                    label: field_label(&f.id, &f.label, f.code.map(|d| d.via)),
                    kind: match f.kind {
                        InputKind::Text => "text",
                        InputKind::Secret => "secret",
                        InputKind::Phone => "phone",
                    }
                    .into(),
                    value: f.default.clone().unwrap_or_default(),
                    required: f.required,
                    note: field_note(f),
                })
                .collect();
            // A new code is offered only where the network allows one (the HIG: an action that
            // does nothing is not shown), and only after the wait it still states.
            if let Some(resend) = delivery.and_then(|d| d.resend) {
                v.resend = trc(
                    "@action:button ask the network for another login code",
                    "Send a New Code",
                )
                .into_owned();
                v.resend_after_ms = u64::try_from(resend.after.as_millis()).unwrap_or(u64::MAX);
            }
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
            // A refusal of the phone number has words of its own: the person acts on it.
            v.error = failure_text(purpose, network, net, error);
            // A second link of an account that is here already would only fail again: no
            // "Try Again". An active account is looked at in the list; a logged-out one is
            // linked again from its banner, which is on the list too.
            match &error.kind {
                kourier_model::ErrorKind::AlreadyAdded { logged_out: false } => {
                    v.retry = false;
                    v.leave = leave_with_ok();
                }
                kourier_model::ErrorKind::AlreadyAdded { logged_out: true } => {
                    v.retry = false;
                    v.leave = trc(
                        "@action:button go to the chats, where the banner of the account that must be linked again is",
                        "Go to Chats",
                    )
                    .into_owned();
                }
                // The network does not offer this way to this account (yet): asking again
                // fails the same; the other way, which the page offers, is the way on.
                // A purchase asked for before a code is SENT to the number (Telegram's `purchase`)
                // is the same: asking again fails, but a QR login sends no code, so "Scan a QR
                // Code Instead" is the way that avoids the purchase (fix round F2).
                kourier_model::ErrorKind::Unsupported
                    if matches!(purpose, "phone" | "purchase") =>
                {
                    v.retry = false
                }
                // Telegram's refusals that no new attempt mends (KT-1b): the number has no
                // account, or the network asks for a newer Kourier first (the failure-only step
                // ids), or the number is banned. Asking again fails the same, and so does the
                // other way to link, so "OK" goes back and nothing else is offered
                // (`CoreSource::with_flows` offers no other flow beside a leave button).
                kourier_model::ErrorKind::Unsupported
                    if matches!(purpose, "registration" | "update") =>
                {
                    v.retry = false;
                    v.leave = leave_with_ok();
                }
                kourier_model::ErrorKind::Unauthorized
                    if purpose == "phone" && network == NetworkKind::Telegram =>
                {
                    v.retry = false;
                    v.leave = leave_with_ok();
                }
                // Threema's first refusal (the profile names no test ID, so no code was shown) is
                // B1's gate: the list is read once at the start, so every new attempt is refused the
                // same way until Kourier restarts in a profile that has one. No "Try Again", then, as
                // for an account that is here already: "OK" goes back (U3). The second refusal (this
                // phone's ID is not on the list) keeps it: another phone can pass.
                kourier_model::ErrorKind::NotTheExpectedAccount
                    if purpose == "qr" && network == NetworkKind::Threema =>
                {
                    v.retry = false;
                    v.leave = leave_with_ok();
                }
                _ => v.retry = true,
            }
        }
    }
    v
}

/// The label of the button that closes a failed link that cannot be tried again and goes to the
/// conversation list.
fn leave_with_ok() -> String {
    trc("@action:button close a message and go to the chats", "OK").into_owned()
}

/// The label of the button that switches the link page to the flow `id`: the viewmodel's words
/// for the flows it knows (`qr`, `pair-code`, Telegram's `phone`), the descriptor's own label for
/// another.
pub fn flow_switch_label(id: &str, descriptor_label: &str) -> String {
    match id {
        // Both flows link with a phone number: the same button, the same message.
        "pair-code" | "phone" => trc(
            "@action:button switch the link page to linking with a phone number",
            "Link with Phone Number Instead",
        )
        .into_owned(),
        "qr" => trc(
            "@action:button switch the link page to scanning a QR code",
            "Scan a QR Code Instead",
        )
        .into_owned(),
        _ => single_line(descriptor_label),
    }
}

/// The text of a failed step by the step it failed at (`purpose`, the step's `step_id`): the
/// number typed for the pair-code flow (`phone`) is refused in ways the person can act on, and
/// Threema's two refusals of a run profile (`qr`: no test ID, `linking`: the ID is not on the
/// list) have their own sentences. Every other step has the general text of [`error_text`].
fn failure_text(purpose: &str, network: NetworkKind, net: &str, error: &BackendError) -> String {
    use kourier_model::ErrorKind;
    match (purpose, &error.kind) {
        ("phone", ErrorKind::RateLimited { retry_after }) => throttled_text(net, *retry_after),
        // Telegram limits the attempts at every step that sends or checks something (the code,
        // the password, the e-mail and its code): the typed wait beats the general "Too many
        // attempts" (KT-1b, LC-KT1b-13). The code step is the number's (only the phone flow has
        // one); the password and the e-mail steps also follow a QR scan, where no number was
        // typed, so their sentence is about the account (post-rebase fix b).
        ("code", ErrorKind::RateLimited { retry_after }) if network == NetworkKind::Telegram => {
            throttled_text(net, *retry_after)
        }
        ("password" | "email" | "email-code", ErrorKind::RateLimited { retry_after })
            if network == NetworkKind::Telegram =>
        {
            throttled_account_text(net, *retry_after)
        }
        // Telegram's refusals. The step ids `registration`, `purchase` and `update` exist only
        // as the step a login FAILED at (the model's vocabulary): no network guard, and the
        // words never come from the error's detail (R4). The banned number and the code sent in
        // a way Kourier cannot take are told by the pair, with the guard: WhatsApp fails at
        // `phone` with `Unauthorized` and at `code` with `Unsupported` too.
        ("phone", ErrorKind::Unauthorized) if network == NetworkKind::Telegram => trfc(
            "@info:status why linking with a phone number failed, %1 the name of a messenger",
            "%1 has banned this phone number. Kourier cannot link it.",
            &[net],
        ),
        ("registration", ErrorKind::Unsupported) => trfc(
            "@info:status why linking with a phone number failed, %1 the name of a messenger",
            "This phone number has no %1 account. Create the account in the %1 app first, then link it here.",
            &[net],
        ),
        ("purchase", ErrorKind::Unsupported) => trfc(
            "@info:status why linking failed, %1 the name of a messenger",
            "%1 asks for a purchase before it sends a code to this number. Scan a QR code instead.",
            &[net],
        ),
        ("update", ErrorKind::Unsupported) => trfc(
            "@info:status why linking failed, %1 the name of a messenger",
            "%1 asks for a newer version of Kourier to log in.",
            &[net],
        ),
        ("code", ErrorKind::Unsupported) if network == NetworkKind::Telegram => trfc(
            "@info:status why linking failed, %1 the name of a messenger",
            "%1 sent the code in a way Kourier cannot take. Scan a QR code instead.",
            &[net],
        ),
        ("email", ErrorKind::Unsupported) if network == NetworkKind::Telegram => trfc(
            "@info:status why linking failed, %1 the name of a messenger",
            "%1 does not accept this email address. Try another address, or scan a QR code instead.",
            &[net],
        ),
        ("phone", ErrorKind::Unsupported) => trfc(
            "@info:status why linking with a phone number failed, %1 the name of a messenger",
            "%1 does not offer linking with a phone number to this account yet. Scan a QR code instead.",
            &[net],
        ),
        ("phone", ErrorKind::InvalidInput) => trc(
            "@info:status why linking with a phone number failed",
            "Check the phone number. Enter it with its country code, for example +380 67 123 4567.",
        )
        .into_owned(),
        // A run that states the account it means to link met another number: nobody scanned a phone
        // here, and a typo is the likely cause, so the words are about the number.
        ("phone", ErrorKind::NotTheExpectedAccount) => trc(
            "@info:status why linking with a phone number failed",
            "This number is not the one this run expects. Check it, or link the expected phone.",
        )
        .into_owned(),
        // Threema links only in a prepared test run profile (E2, QC): the two refusals are the
        // same kind, told apart by the step they happen at. At `qr` the backend refused before it
        // opened a socket (the profile names no test ID: no code was shown); at `linking` the phone
        // had sent its ID and the ID is not on the profile's list.
        ("qr", ErrorKind::NotTheExpectedAccount) if network == NetworkKind::Threema => trc(
            "@info:status why linking failed",
            "Kourier links Threema only in a prepared test profile. No code was shown.",
        )
        .into_owned(),
        ("linking", ErrorKind::NotTheExpectedAccount) if network == NetworkKind::Threema => trc(
            "@info:status why linking failed",
            "This Threema ID is not on Kourier's list of test IDs. Kourier did not link it.",
        )
        .into_owned(),
        _ => error_text(error),
    }
}

/// The network limits the attempts to link this number: how long to wait, rounded up to whole
/// minutes (hours from two hours on). A length of time is a plural piece of a sentence, never a
/// separate message (`notes/uk-translation.md`).
fn throttled_text(net: &str, wait: Option<Duration>) -> String {
    let Some(wait) = wait else {
        return trfc(
            "@info:status why linking with a phone number failed, %1 the name of a messenger",
            "%1 is limiting link attempts for this number. Wait a while before you try again.",
            &[net],
        );
    };
    let minutes = wait.as_secs().div_ceil(60).max(1);
    if minutes < 120 {
        // In a plural `%1` is the count (KI18n fills it before this crate fills the rest), so
        // the network is `%2`.
        trnfc(
            "@info:status why linking with a phone number failed, %1 a number of minutes, %2 the name of a messenger",
            "%2 is limiting link attempts for this number. Wait %1 minute before you try again.",
            "%2 is limiting link attempts for this number. Wait %1 minutes before you try again.",
            minutes,
            &[&minutes.to_string(), net],
        )
    } else {
        let hours = minutes.div_ceil(60);
        trnfc(
            "@info:status why linking with a phone number failed, %1 a number of hours, %2 the name of a messenger",
            "%2 is limiting link attempts for this number. Wait %1 hour before you try again.",
            "%2 is limiting link attempts for this number. Wait %1 hours before you try again.",
            hours,
            &[&hours.to_string(), net],
        )
    }
}

/// The same wait for a step that is not about a number (Telegram's `password`, `email` and
/// `email-code`: they also come after a QR scan, where no number was typed): the account is the
/// one limited. A length of time is a plural piece of a sentence, as in [`throttled_text`].
fn throttled_account_text(net: &str, wait: Option<Duration>) -> String {
    let Some(wait) = wait else {
        return trfc(
            "@info:status why linking failed, %1 the name of a messenger",
            "%1 is limiting link attempts for this account. Wait a while before you try again.",
            &[net],
        );
    };
    let minutes = wait.as_secs().div_ceil(60).max(1);
    if minutes < 120 {
        trnfc(
            "@info:status why linking failed, %1 a number of minutes, %2 the name of a messenger",
            "%2 is limiting link attempts for this account. Wait %1 minute before you try again.",
            "%2 is limiting link attempts for this account. Wait %1 minutes before you try again.",
            minutes,
            &[&minutes.to_string(), net],
        )
    } else {
        let hours = minutes.div_ceil(60);
        trnfc(
            "@info:status why linking failed, %1 a number of hours, %2 the name of a messenger",
            "%2 is limiting link attempts for this account. Wait %1 hour before you try again.",
            "%2 is limiting link attempts for this account. Wait %1 hours before you try again.",
            hours,
            &[&hours.to_string(), net],
        )
    }
}

/// The numbered instructions of a QR code step. Signal's three are the words the link page
/// always had (the same messages as before they moved here); WhatsApp's are WhatsApp Web's, with
/// the menu names of its phone app (checked at the first real link); Threema's are his (QD, TH4)
/// with menu words that are unverified until the first real link (RT1), and Telegram's are the same
/// kind of draft (R1). A network the viewmodel knows nothing about has none: its hint stands alone.
fn qr_steps(network: NetworkKind, net: &str) -> Vec<String> {
    let open = || {
        trfc(
            "@info linking step, %1 is the network",
            "Open %1 on your phone",
            &[net],
        )
    };
    match network {
        NetworkKind::Signal => vec![
            open(),
            trc("@info linking step", "Go to Settings → Linked devices").into_owned(),
            trc(
                "@info linking step",
                "Tap “Link new device” and scan this code",
            )
            .into_owned(),
        ],
        NetworkKind::WhatsApp => vec![
            open(),
            trc(
                "@info linking step",
                "Tap Menu ⋮ (Android) or Settings (iPhone), then “Linked devices”",
            )
            .into_owned(),
            trc(
                "@info linking step",
                "Tap “Link a device” and scan this code",
            )
            .into_owned(),
        ],
        // Threema's menu words are unverified (fixed at the first real link, RT1). The code does
        // not rotate, so there is no line about that (QD).
        NetworkKind::Threema => vec![
            open(),
            trc("@info linking step", "Go to Settings → Linked Devices").into_owned(),
            trc(
                "@info linking step",
                "Tap the button to link a device and scan this code",
            )
            .into_owned(),
        ],
        // Telegram's menu words are unverified until the first real link (R1), as Threema's.
        // Its token DOES rotate (`rotates` stays true).
        NetworkKind::Telegram => vec![
            open(),
            trc("@info linking step", "Go to Settings → Devices").into_owned(),
            trc(
                "@info linking step",
                "Tap “Link Desktop Device” and scan this code",
            )
            .into_owned(),
        ],
        _ => Vec::new(),
    }
}

/// The numbered instructions of the step that shows a code to type on the phone (WhatsApp's
/// "link with phone number").
fn code_steps(net: &str) -> Vec<String> {
    vec![
        trfc(
            "@info linking step, %1 is the network",
            "Open %1 on your phone",
            &[net],
        ),
        trc(
            "@info linking step",
            "Tap Menu ⋮ or Settings, then “Linked devices” and “Link a device”",
        )
        .into_owned(),
        trc(
            "@info linking step",
            "Tap “Link with phone number instead” and type this code",
        )
        .into_owned(),
    ]
}

/// The name of one of the 128 emoji of Threema's linking wizard (QB: short, capitalised, one or
/// two words like a label), by the glyph exactly as the backend sends it: Desktop's code points
/// (`apps/desktop/src/app/ui/linking/emoji-list.ts`, the same list the phone uses), the variation
/// selector U+FE0F kept where Desktop has it. The words are CLDR's emoji annotations (the names
/// Plasma's emoji picker shows) cut to the label (`target/threema/th4-k2a-names.md` has the
/// table). `None` for any other glyph: the page then shows the glyph alone.
pub fn emoji_name(glyph: &str) -> Option<String> {
    let name = match glyph {
        // 🐵 (emoji-list.ts:3)
        "\u{1F435}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Monkey",
        ),
        // 🐶 (emoji-list.ts:4)
        "\u{1F436}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Dog",
        ),
        // 🐩 (emoji-list.ts:5)
        "\u{1F429}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Poodle",
        ),
        // 🦊 (emoji-list.ts:6)
        "\u{1F98A}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Fox",
        ),
        // 🐈‍⬛ (emoji-list.ts:7)
        "\u{1F408}\u{200D}\u{2B1B}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Black Cat",
        ),
        // 🐴 (emoji-list.ts:8)
        "\u{1F434}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Horse",
        ),
        // 🦬 (emoji-list.ts:9)
        "\u{1F9AC}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Bison",
        ),
        // 🐮 (emoji-list.ts:10)
        "\u{1F42E}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Cow",
        ),
        // 🐷 (emoji-list.ts:11)
        "\u{1F437}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Pig",
        ),
        // 🐪 (emoji-list.ts:12)
        "\u{1F42A}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Camel",
        ),
        // 🦒 (emoji-list.ts:13)
        "\u{1F992}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Giraffe",
        ),
        // 🐘 (emoji-list.ts:14)
        "\u{1F418}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Elephant",
        ),
        // 🐭 (emoji-list.ts:15)
        "\u{1F42D}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Mouse",
        ),
        // 🐰 (emoji-list.ts:16)
        "\u{1F430}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Rabbit",
        ),
        // 🐿️ (emoji-list.ts:17)
        "\u{1F43F}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Chipmunk",
        ),
        // 🐼 (emoji-list.ts:18)
        "\u{1F43C}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Panda",
        ),
        // 🦘 (emoji-list.ts:19)
        "\u{1F998}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Kangaroo",
        ),
        // 🐔 (emoji-list.ts:20)
        "\u{1F414}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Chicken",
        ),
        // 🕊️ (emoji-list.ts:21)
        "\u{1F54A}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Dove",
        ),
        // 🦆 (emoji-list.ts:22)
        "\u{1F986}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Duck",
        ),
        // 🦢 (emoji-list.ts:23)
        "\u{1F9A2}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Swan",
        ),
        // 🦉 (emoji-list.ts:24)
        "\u{1F989}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Owl",
        ),
        // 🦤 (emoji-list.ts:25)
        "\u{1F9A4}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Dodo",
        ),
        // 🪶 (emoji-list.ts:26)
        "\u{1FAB6}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Feather",
        ),
        // 🦩 (emoji-list.ts:27)
        "\u{1F9A9}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Flamingo",
        ),
        // 🐸 (emoji-list.ts:28)
        "\u{1F438}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Frog",
        ),
        // 🐋 (emoji-list.ts:29)
        "\u{1F40B}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Whale",
        ),
        // 🐬 (emoji-list.ts:30)
        "\u{1F42C}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Dolphin",
        ),
        // 🦭 (emoji-list.ts:31)
        "\u{1F9AD}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Seal",
        ),
        // 🐟 (emoji-list.ts:32)
        "\u{1F41F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Fish",
        ),
        // 🐚 (emoji-list.ts:33)
        "\u{1F41A}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Spiral Shell",
        ),
        // 🪲 (emoji-list.ts:34)
        "\u{1FAB2}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Beetle",
        ),
        // 🕸️ (emoji-list.ts:35)
        "\u{1F578}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Spider Web",
        ),
        // 🏵️ (emoji-list.ts:36)
        "\u{1F3F5}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Rosette",
        ),
        // 🌻 (emoji-list.ts:37)
        "\u{1F33B}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Sunflower",
        ),
        // 🌲 (emoji-list.ts:38)
        "\u{1F332}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Evergreen Tree",
        ),
        // 🌿 (emoji-list.ts:39)
        "\u{1F33F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Herb",
        ),
        // ☘️ (emoji-list.ts:40)
        "\u{2618}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Shamrock",
        ),
        // 🍉 (emoji-list.ts:41)
        "\u{1F349}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Watermelon",
        ),
        // 🍋 (emoji-list.ts:42)
        "\u{1F34B}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Lemon",
        ),
        // 🍎 (emoji-list.ts:43)
        "\u{1F34E}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Red Apple",
        ),
        // 🍒 (emoji-list.ts:44)
        "\u{1F352}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Cherries",
        ),
        // 🍓 (emoji-list.ts:45)
        "\u{1F353}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Strawberry",
        ),
        // 🫐 (emoji-list.ts:46)
        "\u{1FAD0}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Blueberries",
        ),
        // 🍅 (emoji-list.ts:47)
        "\u{1F345}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Tomato",
        ),
        // 🥑 (emoji-list.ts:48)
        "\u{1F951}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Avocado",
        ),
        // 🥕 (emoji-list.ts:49)
        "\u{1F955}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Carrot",
        ),
        // 🥦 (emoji-list.ts:50)
        "\u{1F966}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Broccoli",
        ),
        // 🍄 (emoji-list.ts:51)
        "\u{1F344}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Mushroom",
        ),
        // 🫘 (emoji-list.ts:52)
        "\u{1FAD8}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Beans",
        ),
        // 🧂 (emoji-list.ts:53)
        "\u{1F9C2}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Salt",
        ),
        // 🍪 (emoji-list.ts:54)
        "\u{1F36A}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Cookie",
        ),
        // 🍫 (emoji-list.ts:55)
        "\u{1F36B}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Chocolate Bar",
        ),
        // ☕ (emoji-list.ts:56)
        "\u{2615}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Hot Beverage",
        ),
        // 🧊 (emoji-list.ts:57)
        "\u{1F9CA}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Ice Cube",
        ),
        // 🥢 (emoji-list.ts:58)
        "\u{1F962}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Chopsticks",
        ),
        // 🗺️ (emoji-list.ts:59)
        "\u{1F5FA}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "World Map",
        ),
        // 🌋 (emoji-list.ts:60)
        "\u{1F30B}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Volcano",
        ),
        // 🏕️ (emoji-list.ts:61)
        "\u{1F3D5}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Camping",
        ),
        // 🏝️ (emoji-list.ts:62)
        "\u{1F3DD}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Desert Island",
        ),
        // 🏛️ (emoji-list.ts:63)
        "\u{1F3DB}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Classical Building",
        ),
        // 🚂 (emoji-list.ts:64)
        "\u{1F682}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Locomotive",
        ),
        // 🚚 (emoji-list.ts:65)
        "\u{1F69A}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Delivery Truck",
        ),
        // 🚜 (emoji-list.ts:66)
        "\u{1F69C}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Tractor",
        ),
        // 🛤️ (emoji-list.ts:67)
        "\u{1F6E4}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Railway Track",
        ),
        // ⚓ (emoji-list.ts:68)
        "\u{2693}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Anchor",
        ),
        // 🛰️ (emoji-list.ts:69)
        "\u{1F6F0}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Satellite",
        ),
        // 🚀 (emoji-list.ts:70)
        "\u{1F680}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Rocket",
        ),
        // 🌙 (emoji-list.ts:71)
        "\u{1F319}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Crescent Moon",
        ),
        // ☀️ (emoji-list.ts:72)
        "\u{2600}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Sun",
        ),
        // ⭐ (emoji-list.ts:73)
        "\u{2B50}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Star",
        ),
        // 🌈 (emoji-list.ts:74)
        "\u{1F308}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Rainbow",
        ),
        // ☂️ (emoji-list.ts:75)
        "\u{2602}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Umbrella",
        ),
        // ❄️ (emoji-list.ts:76)
        "\u{2744}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Snowflake",
        ),
        // ☃️ (emoji-list.ts:77)
        "\u{2603}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Snowman",
        ),
        // 🔥 (emoji-list.ts:78)
        "\u{1F525}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Fire",
        ),
        // 💧 (emoji-list.ts:79)
        "\u{1F4A7}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Droplet",
        ),
        // ✨ (emoji-list.ts:80)
        "\u{2728}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Sparkles",
        ),
        // 🎈 (emoji-list.ts:81)
        "\u{1F388}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Balloon",
        ),
        // 🎀 (emoji-list.ts:82)
        "\u{1F380}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Ribbon",
        ),
        // 🥇 (emoji-list.ts:83)
        "\u{1F947}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Gold Medal",
        ),
        // 🏀 (emoji-list.ts:84)
        "\u{1F3C0}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Basketball",
        ),
        // 🏐 (emoji-list.ts:85)
        "\u{1F3D0}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Volleyball",
        ),
        // 🎳 (emoji-list.ts:86)
        "\u{1F3B3}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Bowling",
        ),
        // 🏓 (emoji-list.ts:87)
        "\u{1F3D3}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Ping Pong",
        ),
        // ⛳ (emoji-list.ts:88)
        "\u{26F3}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Golf Flag",
        ),
        // 🎯 (emoji-list.ts:89)
        "\u{1F3AF}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Bullseye",
        ),
        // 🕹️ (emoji-list.ts:90)
        "\u{1F579}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Joystick",
        ),
        // 🧩 (emoji-list.ts:91)
        "\u{1F9E9}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Puzzle Piece",
        ),
        // 🧸 (emoji-list.ts:92)
        "\u{1F9F8}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Teddy Bear",
        ),
        // ♠️ (emoji-list.ts:93)
        "\u{2660}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Spade Suit",
        ),
        // ♥️ (emoji-list.ts:94)
        "\u{2665}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Heart Suit",
        ),
        // 👗 (emoji-list.ts:95)
        "\u{1F457}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Dress",
        ),
        // 👑 (emoji-list.ts:96)
        "\u{1F451}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Crown",
        ),
        // 🔔 (emoji-list.ts:97)
        "\u{1F514}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Bell",
        ),
        // 🎷 (emoji-list.ts:98)
        "\u{1F3B7}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Saxophone",
        ),
        // 🎸 (emoji-list.ts:99)
        "\u{1F3B8}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Guitar",
        ),
        // 🖨️ (emoji-list.ts:100)
        "\u{1F5A8}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Printer",
        ),
        // 📸 (emoji-list.ts:101)
        "\u{1F4F8}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Camera",
        ),
        // 🕯️ (emoji-list.ts:102)
        "\u{1F56F}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Candle",
        ),
        // 📖 (emoji-list.ts:103)
        "\u{1F4D6}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Open Book",
        ),
        // 📦 (emoji-list.ts:104)
        "\u{1F4E6}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Package",
        ),
        // 📮 (emoji-list.ts:105)
        "\u{1F4EE}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Postbox",
        ),
        // 📝 (emoji-list.ts:106)
        "\u{1F4DD}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Memo",
        ),
        // 💼 (emoji-list.ts:107)
        "\u{1F4BC}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Briefcase",
        ),
        // 📋 (emoji-list.ts:108)
        "\u{1F4CB}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Clipboard",
        ),
        // 🔒 (emoji-list.ts:109)
        "\u{1F512}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Lock",
        ),
        // 🔑 (emoji-list.ts:110)
        "\u{1F511}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Key",
        ),
        // ⚒️ (emoji-list.ts:111)
        "\u{2692}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Hammer and Pick",
        ),
        // 🪃 (emoji-list.ts:112)
        "\u{1FA83}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Boomerang",
        ),
        // ⚖️ (emoji-list.ts:113)
        "\u{2696}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Balance Scale",
        ),
        // 🔗 (emoji-list.ts:114)
        "\u{1F517}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Chain Link",
        ),
        // 🪝 (emoji-list.ts:115)
        "\u{1FA9D}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Hook",
        ),
        // 🔬 (emoji-list.ts:116)
        "\u{1F52C}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Microscope",
        ),
        // 🪑 (emoji-list.ts:117)
        "\u{1FA91}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Chair",
        ),
        // 🚽 (emoji-list.ts:118)
        "\u{1F6BD}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Toilet",
        ),
        // 🧹 (emoji-list.ts:119)
        "\u{1F9F9}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Broom",
        ),
        // 🪣 (emoji-list.ts:120)
        "\u{1FAA3}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Bucket",
        ),
        // 🫧 (emoji-list.ts:121)
        "\u{1FAE7}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Bubbles",
        ),
        // ⚫ (emoji-list.ts:122)
        "\u{26AB}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Black Circle",
        ),
        // 🟨 (emoji-list.ts:123)
        "\u{1F7E8}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Yellow Square",
        ),
        // ▶️ (emoji-list.ts:124)
        "\u{25B6}\u{FE0F}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Play Button",
        ),
        // 📶 (emoji-list.ts:125)
        "\u{1F4F6}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Antenna Bars",
        ),
        // 💥 (emoji-list.ts:126)
        "\u{1F4A5}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Collision",
        ),
        // 💬 (emoji-list.ts:127)
        "\u{1F4AC}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Speech Balloon",
        ),
        // 💫 (emoji-list.ts:128)
        "\u{1F4AB}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Dizzy",
        ),
        // 👀 (emoji-list.ts:129)
        "\u{1F440}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Eyes",
        ),
        // 👣 (emoji-list.ts:130)
        "\u{1F463}" => trc(
            "@label name of an emoji the phone shows when linking",
            "Footprints",
        ),
        _ => return None,
    };
    Some(name.into_owned())
}

/// The line above the fields of an input step, keyed on the network AND the step's purpose: the
/// device name (Signal's first step), the phone number (WhatsApp's pair-code flow has its own two
/// sentences, every other network the one that names it), where a login code went (`delivery`:
/// the code field's typed way, `None` = the network said nothing), the password and the e-mail of
/// Telegram's steps and the four retry ids, whose words say that the last entry was not right.
/// Other steps have none.
fn input_explain(network: NetworkKind, purpose: &str, delivery: Option<CodeVia>) -> String {
    let net = network_name(network);
    match purpose {
        "device-name" => trc(
            "@info",
            "Your phone shows this name in its list of linked devices.",
        )
        .into_owned(),
        "phone" if network == NetworkKind::WhatsApp => format!(
            "{}\n\n{}",
            trc(
                "@info what to type in the phone number field",
                "Enter the phone number of your WhatsApp account with its country code, for example +380 67 123 4567.",
            ),
            // WhatsApp accepts only a browser name for a device linked this way.
            trc(
                "@info what the phone lists after linking with a phone number",
                "Your phone will list this computer as “Chrome (Linux)”: WhatsApp accepts only a browser name when you link with a phone number.",
            )
        ),
        "phone" => trfc(
            "@info what to type in the phone number field, %1 the name of a messenger",
            "Enter the phone number of your %1 account with its country code, for example +380 67 123 4567.",
            &[net],
        ),
        "code" => match delivery {
            Some(CodeVia::App) => trfc(
                "@info where the network sent the login code, %1 the name of a messenger",
                "%1 sent the code to your %1 app on another device.",
                &[net],
            ),
            Some(CodeVia::Sms) => trfc(
                "@info where the network sent the login code, %1 the name of a messenger",
                "%1 sent the code by SMS.",
                &[net],
            ),
            Some(CodeVia::SmsWord) => trfc(
                "@info where the network sent the login code, %1 the name of a messenger",
                "%1 sent a word by SMS. Enter that word.",
                &[net],
            ),
            Some(CodeVia::SmsPhrase) => trfc(
                "@info where the network sent the login code, %1 the name of a messenger",
                "%1 sent a phrase by SMS. Enter that phrase.",
                &[net],
            ),
            Some(CodeVia::Call) => trfc(
                "@info where the network sent the login code, %1 the name of a messenger",
                "%1 is calling you to tell you the code.",
                &[net],
            ),
            Some(CodeVia::Fragment) => trfc(
                "@info where the network sent the login code, %1 the name of a messenger",
                "%1 sent the code to your anonymous number on Fragment.",
                &[net],
            ),
            Some(CodeVia::Other) | None => trfc(
                "@info where the network sent the login code, %1 the name of a messenger",
                "Enter the code %1 sent you.",
                &[net],
            ),
        },
        "email-code" => trfc(
            "@info where the network sent the login code, %1 the name of a messenger",
            "Enter the code %1 sent to your email address.",
            &[net],
        ),
        "code-again" | "email-code-again" => trc(
            "@info the login code that was entered last was wrong",
            "That code is not right. Check it and enter it again.",
        )
        .into_owned(),
        "password" => format!(
            "{}\n\n{}",
            trc(
                "@info what to type in the password field of a login",
                "This account has two-step verification. Enter its password.",
            ),
            forgot_password_line(net)
        ),
        // The moment the person most needs the way back is after a wrong password (fix round F5).
        "password-again" => format!(
            "{}\n\n{}",
            trc(
                "@info the login password that was entered last was wrong",
                "That password is not right. Try again.",
            ),
            forgot_password_line(net)
        ),
        "email" => trfc(
            "@info what to type in the email field of a login, %1 the name of a messenger",
            "%1 asks for an email address for this account's logins. It sends a code there next.",
            &[net],
        ),
        "email-again" => trfc(
            "@info the email address that was entered last was refused, %1 the name of a messenger",
            "%1 does not accept that address. Check it and try again.",
            &[net],
        ),
        _ => String::new(),
    }
}

/// What to do about a forgotten login password: Kourier cannot reset it, the network's own app can.
fn forgot_password_line(net: &str) -> String {
    trfc(
        "@info how to get a forgotten login password back, %1 the name of a messenger",
        "Forgot it? Reset it in the %1 app: Kourier cannot.",
        &[net],
    )
}

/// The label of a field: the viewmodel's words for the ids it knows, the backend's otherwise.
/// The code field is a "Word" or a "Phrase" where the network sent one by SMS.
fn field_label(id: &str, backend: &str, via: Option<CodeVia>) -> String {
    match (id, via) {
        ("device-name", _) => trc("@label:textbox", "Device name").into_owned(),
        ("phone", _) => trc("@label:textbox", "Phone number").into_owned(),
        ("code", Some(CodeVia::SmsWord)) => trc("@label:textbox", "Word").into_owned(),
        ("code", Some(CodeVia::SmsPhrase)) => trc("@label:textbox", "Phrase").into_owned(),
        ("code", _) => trc("@label:textbox", "Code").into_owned(),
        ("password", _) => trc("@label:textbox", "Password").into_owned(),
        ("email", _) => trc("@label:textbox", "Email address").into_owned(),
        _ => single_line(backend),
    }
}

/// The quiet line under a field, from what the network itself said about it (`InputField::note`:
/// foreign text, under the rules of a network's note): its text as ONE cut line of plain text
/// inside the viewmodel's own words, empty when it said nothing or the field has no words for a
/// note. A password field shows Telegram's hint, the code field of a word or phrase sent by SMS
/// shows the first letter or word. (Bidi format characters are not stripped here: the backend
/// does that for the notes it builds.)
fn field_note(field: &InputField) -> String {
    let Some(text) = field
        .note
        .as_ref()
        .and_then(|n| n.explanation.as_deref())
        .map(note_line)
        .filter(|t| !t.is_empty())
    else {
        return String::new();
    };
    match (field.id.as_str(), field.code.map(|d| d.via)) {
        ("password", _) => trfc(
            "@info the hint the account owner gave their password, %1 the hint",
            "Hint: %1",
            &[&text],
        ),
        ("code", Some(CodeVia::SmsWord | CodeVia::SmsPhrase)) => trfc(
            "@info the first letter or word of a login code sent by SMS, %1 that letter or word",
            "It starts with “%1”.",
            &[&text],
        ),
        _ => String::new(),
    }
}

/// A pairing code as the person types it: eight characters are shown as `XXXX-XXXX`; any other
/// code (the fake's, which has its hyphen already) is shown as it is.
fn display_code(code: &str) -> String {
    if code.chars().count() == 8 && code.chars().all(|c| c.is_ascii_alphanumeric()) {
        let (a, b) = code.split_at(4);
        format!("{a}-{b}")
    } else {
        code.to_owned()
    }
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
        // The wrong phone scanned (or its code entered) during a run that states which account it
        // means to link: Kourier did not link it (what the phone lists after the refusal is not
        // Kourier's to say), and the person is told whose fault it is. A number TYPED at the `phone`
        // step has its own words in `failure_text`.
        ErrorKind::NotTheExpectedAccount => trc(
            "@info:status why linking failed",
            "This phone belongs to another account than the one this run expects. Kourier did not link it.",
        ),
        ErrorKind::Unauthorized | ErrorKind::LoggedOut => trc(
            "@info:status why linking failed",
            "The phone rejected the link.",
        ),
        ErrorKind::TooManyDevices { max: Some(max) } => trfc(
            "@info:status why linking failed, %1 how many linked devices the network allows",
            "This account already has as many linked devices as the network allows (%1). Remove one in the linked devices on your phone, then try again.",
            &[&max.to_string()],
        )
        .into(),
        ErrorKind::TooManyDevices { max: None } => trc(
            "@info:status why linking failed",
            "This account already has as many linked devices as the network allows. Remove one in the linked devices on your phone, then try again.",
        ),
        _ => trc("@info:status why linking failed", "Something went wrong."),
    }
    .into_owned()
}

/// A network the build holds, as the welcome page needs it. Plain data: the front-end builds it
/// from what the application tells (the registered networks, the experimental ones that are
/// turned off), so the choice of rows, their texts and the warning stay below the seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkEntry {
    /// The network.
    pub kind: NetworkKind,
    /// Its login flows as (id, label), the first one is the default (`qr`, `pair-code`). The
    /// label is the descriptor's own: the fallback for a flow the viewmodel has no words for.
    pub flows: Vec<(String, String)>,
    /// Experimental: registered only while the user has it turned on.
    pub experimental: bool,
    /// The descriptor's warning (title, body), if it has one: the person is asked before the
    /// first account is added. The viewmodel has its own words for the networks it knows; this
    /// text is shown for a network it does not.
    pub risk_notice: Option<(String, String)>,
    /// A fresh link can bring the account's history as one archive (the descriptor's
    /// `history_transfer`).
    pub history_transfer: bool,
    /// Where the network's older history comes from, as its descriptor says: what the
    /// welcome page's footnote may promise ([`welcome_footnote`]).
    pub history: HistorySource,
    /// Registered at this start (an experimental network is off when it is not).
    pub on: bool,
    /// The network's accounts always send read receipts and typing (its descriptor's
    /// `receipts_always_sent`): Settings says so ([`privacy_note`]).
    pub receipts_always_sent: bool,
    /// This build cannot log in to the network (Telegram without its API key or library) and
    /// says why: the entry is listed in Settings without a switch, never in Add Account, and its
    /// accounts, on or off, show the banner of a missing part ([`NetworkMissing`]). The
    /// application feeds it for the network whenever the build cannot log in to it.
    pub missing: Option<NetworkMissing>,
}

/// Why a build cannot log in to a network (Telegram: no API key compiled in, or the TDLib part
/// absent). Plain data of the viewmodel: the application maps its own typed value onto it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetworkMissing {
    /// The build has no API key for the network.
    ApiKey,
    /// The network's library is missing from the installation.
    Library,
}

impl NetworkEntry {
    /// A registered network that is not experimental, with the default flow and no warning.
    pub fn plain(kind: NetworkKind) -> Self {
        Self {
            kind,
            flows: vec![("qr".to_owned(), "QR code".to_owned())],
            experimental: false,
            risk_notice: None,
            history_transfer: false,
            history: HistorySource::None,
            on: true,
            receipts_always_sent: false,
            missing: None,
        }
    }
}

/// The line of a network's row when this build cannot log in to it.
fn missing_description() -> String {
    trc(
        "@info:status a messenger that cannot be linked",
        "Not available in this build",
    )
    .into_owned()
}

/// The line of an experimental network's row while the network is off.
fn off_description() -> String {
    trc(
        "@info:status a messenger that is in this build, experimental and turned off",
        "Off · experimental",
    )
    .into_owned()
}

/// The question "Turn Off WhatsApp?" says what stays and what the network does with a device
/// that stays unused. The period is the network's own (WhatsApp logs a linked device out after
/// about 30 days; Telegram ends a session by the time its own app sets); a network the viewmodel
/// has no number for says it vaguely.
fn turn_off_text(kind: NetworkKind) -> String {
    let name = network_name(kind);
    match kind {
        NetworkKind::WhatsApp => trfc(
            "@info, %1 the name of a messenger",
            "Kourier stops connecting to your %1 account. It stays linked, also on your phone, and its chats stay here; turn %1 on again to send and receive. %1 removes a linked device that has not connected for about 30 days.",
            &[name],
        ),
        // Telegram ends a session that has not connected for the time set in its app (its
        // default is [U] until the first real link, R1).
        NetworkKind::Telegram => trfc(
            "@info, %1 the name of a messenger",
            "Kourier stops connecting to your %1 account. Its session stays, and its chats stay here; turn %1 on again to send and receive. %1 ends a session that has not connected for the time set in the %1 app (six months unless changed).",
            &[name],
        ),
        _ => trfc(
            "@info, %1 the name of a messenger",
            "Kourier stops connecting to your %1 account. It stays linked, also on your phone, and its chats stay here; turn %1 on again to send and receive. %1 may remove a linked device that has not connected for a long time.",
            &[name],
        ),
    }
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
    /// The row can be clicked: the network is in this build and either on or an experimental one
    /// that the warning page can turn on.
    pub available: bool,
    /// A login can start now: the network is registered at this start.
    pub on: bool,
    /// The network is experimental (off until the user turns it on).
    pub experimental: bool,
    /// A new account is only added after a warning the person accepts
    /// ([`NetworkChoice::risk_view`]).
    pub asks_first: bool,
    /// The descriptor's own warning (title, body), the fallback for a network the viewmodel
    /// has no words for.
    pub risk_notice: Option<(String, String)>,
    /// The login flows as (id, label), the first is the default.
    pub flows: Vec<(String, String)>,
    /// A fresh link can bring the account's history as ONE archive the phone uploads (the
    /// descriptor's `history_transfer`): an account of it offers "Transfer History from
    /// Phone…". Never keyed on `HistorySource`: WhatsApp's descriptor says `PushedOnLink` too
    /// (the phone pushes history in pieces), and that is no archive to transfer.
    pub history_transfer: bool,
    /// Where the network's older history comes from (its descriptor's `history`): the welcome
    /// page's footnote promises a copy of the history only for a row that can be chosen and
    /// has a source ([`welcome_footnote`]).
    pub history: HistorySource,
    /// What the row of an experimental network says while it is OFF (the preference, which
    /// can differ from this start's registry: K-A fix U7); empty for any other network.
    pub off_description: String,
    /// The question's text when the network is turned off in Settings with an account of it:
    /// what stays, and what the network does with a device that stays unused (its own period,
    /// keyed on the kind: K-A fix U5).
    pub turn_off_text: String,
    /// The network's accounts always send read receipts and typing ([`privacy_note`]).
    pub receipts_always_sent: bool,
    /// This build cannot log in to the network ([`NetworkEntry::missing`]): the row is for
    /// Settings, which lists it without a switch; it is no choice of Add Account or the welcome
    /// page (`available` and `on` are false, `flows` is empty). A front-end that lists rows
    /// skips it there.
    pub missing: Option<NetworkMissing>,
}

/// The rows of the welcome page and of "Add Account": Signal first (a row that says so when it
/// is not in the build), the test network second, then the others in registration order. An
/// experimental network is always listed once it is in the build: "Off · experimental" while it
/// is not registered at this start (its row opens the warning page, which turns it on), and
/// with its description while it is on. A network that is not in the build has no row (no
/// "Coming later" rows: an action that does nothing is not shown). `fixture` says why Signal
/// is missing from a test run.
pub fn network_choices(entries: &[NetworkEntry], fixture: bool) -> Vec<NetworkChoice> {
    let describe = |e: &NetworkEntry| -> String {
        let name = network_name(e.kind);
        match e.kind {
            // The build cannot log in to it, on or off: Settings lists the row and says so (his
            // answer to U-1: "Settings row + banner").
            _ if e.missing.is_some() => missing_description(),
            NetworkKind::Signal => trc(
                "@info a messenger that can be linked",
                "Link as a secondary device of the Signal app on your phone",
            )
            .into_owned(),
            NetworkKind::Fake => trc(
                "@info a test account that can be linked",
                "A scripted demo account with sample chats",
            )
            .into_owned(),
            _ if e.experimental && !e.on => off_description(),
            _ if e.experimental => trfc(
                "@info a messenger that can be linked, still experimental, %1 its name",
                "Link as a secondary device of the %1 app on your phone (experimental)",
                &[name],
            ),
            _ => trfc(
                "@info a messenger that can be linked, %1 its name",
                "Link as a secondary device of the %1 app on your phone",
                &[name],
            ),
        }
    };
    let row = |e: &NetworkEntry| {
        // A network the build cannot log in to is no choice: nothing starts, nothing is warned
        // about (turning it on would not help).
        let absent = e.missing.is_some();
        NetworkChoice {
            kind: e.kind,
            name: network_name(e.kind).to_owned(),
            description: describe(e),
            icon: network_icon(e.kind).to_owned(),
            // An experimental network that is off can be chosen: the warning page turns it on.
            available: !absent && (e.on || e.experimental),
            on: e.on && !absent,
            experimental: e.experimental,
            asks_first: !absent && e.risk_notice.is_some(),
            risk_notice: e.risk_notice.clone().filter(|_| !absent),
            flows: if absent { Vec::new() } else { e.flows.clone() },
            history_transfer: e.history_transfer,
            history: e.history,
            off_description: if absent {
                missing_description()
            } else if e.experimental {
                off_description()
            } else {
                String::new()
            },
            turn_off_text: turn_off_text(e.kind),
            receipts_always_sent: e.receipts_always_sent,
            missing: e.missing,
        }
    };
    let mut out = Vec::new();
    out.push(
        match entries.iter().find(|e| e.kind == NetworkKind::Signal) {
            Some(e) => row(e),
            None => {
                let mut missing = row(&NetworkEntry::plain(NetworkKind::Signal));
                missing.available = false;
                missing.on = false;
                missing.flows.clear();
                missing.description = if fixture {
                    trc(
                        "@info:status a messenger that cannot be linked",
                        "Not available in fixture mode",
                    )
                } else {
                    trc(
                        "@info:status a messenger that cannot be linked",
                        "Not in this build",
                    )
                }
                .into_owned();
                missing
            }
        },
    );
    let others = |fake: bool| {
        entries
            .iter()
            .filter(move |e| e.kind != NetworkKind::Signal && (e.kind == NetworkKind::Fake) == fake)
            .map(row)
    };
    out.extend(others(true));
    out.extend(others(false));
    out
}

/// The footnote under the welcome page's list of networks. It says what a linked device is, and
/// that the phone can send a copy of its history only when a network the person can choose now
/// (an enabled row) has a history source (U7: Threema sends no history to a new device, so a
/// list of Threema alone must not promise one), and says only that Kourier links as an additional
/// device when every row the person can choose keeps its history on the network's servers
/// (`OnDemand`: Telegram has no main phone that keeps it). The choice is made here from the
/// descriptors' typed data; the page only shows the text.
pub fn welcome_footnote(choices: &[NetworkChoice]) -> String {
    // Only a network whose phone PUSHES the history at the link can "send a copy" of it: an
    // `OnDemand` network (Telegram) serves older messages from its servers when they are asked
    // for, and no phone sends anything (KT-1b, finding 3; the same principle as U7).
    let can_send_a_copy = choices
        .iter()
        .any(|c| c.available && c.history == HistorySource::PushedOnLink);
    // A list whose every choosable row is `OnDemand` (Telegram alone): there is no main phone
    // that keeps the history either (all sessions are peers, the servers keep the history), so
    // both clauses of the short sentence would be false; only the first half is true (KT-1b fix
    // round F3).
    let only_on_demand = choices.iter().any(|c| c.available)
        && choices
            .iter()
            .filter(|c| c.available)
            .all(|c| c.history == HistorySource::OnDemand);
    if can_send_a_copy {
        trc(
            "@info",
            "Kourier links as an additional device: your phone stays the main device, keeps your message history and can send a copy of it.",
        )
    } else if only_on_demand {
        trc("@info", "Kourier links as an additional device.")
    } else {
        trc(
            "@info",
            "Kourier links as an additional device: your phone stays the main device and keeps your message history.",
        )
    }
    .into_owned()
}

/// The sentence Settings adds under "Read receipts" and "Typing indicators": one "Always sent for
/// Telegram." per network that is ON at this start and always sends them (its descriptor's
/// `receipts_always_sent`; Telegram's API rules, HA TG6), joined by a space; empty when no
/// such network is on. The page shows it on a new line only when it is not empty. Computed from
/// typed data once, like [`welcome_footnote`].
pub fn privacy_note(choices: &[NetworkChoice]) -> String {
    choices
        .iter()
        .filter(|c| c.on && c.receipts_always_sent)
        .map(|c| {
            trfc(
                "@info a network that always sends read receipts and typing, %1 its name",
                "Always sent for %1.",
                &[&c.name],
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The warning before a new account of a network is added, as the page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RiskView {
    /// Page title.
    pub title: String,
    /// The paragraphs, plain text.
    pub paragraphs: Vec<String>,
    /// The label of the box the person ticks to go on (empty: none; never ticked before, never
    /// remembered).
    pub confirm: String,
    /// The button that goes on.
    pub accept: String,
    /// The button that goes back.
    pub cancel: String,
    /// What the disabled `accept` button says to the screen reader: why it does not work.
    pub accept_hint: String,
    /// A line under the box when going on first turns the network on (it restarts Kourier); empty
    /// when the network is on already.
    pub restart_note: String,
}

impl NetworkChoice {
    /// The warning to show before a new account of this network is added; `None` when the
    /// network asks nothing first. Always the viewmodel's own words for the networks it knows
    /// (translated); the descriptor's text for a network it does not.
    pub fn risk_view(&self) -> Option<RiskView> {
        if !self.asks_first {
            return None;
        }
        let cancel = trc("@action:button go back without linking", "Cancel").into_owned();
        let accept_hint = trc(
            "@info:tooltip why the button does not work yet",
            "Tick the box above to continue.",
        )
        .into_owned();
        let accept = if self.on {
            trc("@action:button go on to link the account", "Continue")
        } else {
            trc(
                "@action:button turn the network on, restart Kourier and go on to link the account",
                "Turn On and Restart",
            )
        }
        .into_owned();
        let restart_note = if self.on {
            String::new()
        } else {
            trfc(
                "@info why a button restarts the application, %1 the name of a messenger",
                "Kourier has to start again to turn %1 on. It then opens the page that links the account.",
                &[&self.name],
            )
        };
        if self.kind == NetworkKind::WhatsApp {
            return Some(RiskView {
                title: trc(
                    "@title:window warning before linking a WhatsApp account",
                    "Before You Link WhatsApp",
                )
                .into_owned(),
                paragraphs: vec![
                    trc(
                        "@info warning before linking WhatsApp",
                        "Kourier is not made or approved by WhatsApp. It links to your account as an additional device, as WhatsApp Web does, but WhatsApp's terms do not allow unofficial apps.",
                    )
                    .into_owned(),
                    trc(
                        "@info warning before linking WhatsApp",
                        "WhatsApp may restrict or ban an account it finds using Kourier, for some hours or for good. This has happened to people using apps like this, also with ordinary use.",
                    )
                    .into_owned(),
                    trc(
                        "@info warning before linking WhatsApp",
                        "Kourier never starts a new chat. To write to someone new, start the chat on your phone.",
                    )
                    .into_owned(),
                    trc(
                        "@info warning before linking WhatsApp",
                        "If you can, use a separate phone number for WhatsApp in Kourier, not an account you cannot lose.",
                    )
                    .into_owned(),
                    // The exact label the phone shows is not promised: it is what WhatsApp
                    // makes of the name the link was made with.
                    trc(
                        "@info warning before linking WhatsApp",
                        "Your phone lists this computer as a linked device. No usage statistics are sent to WhatsApp.",
                    )
                    .into_owned(),
                ],
                confirm: trc(
                    "@option:check the person understands the risk of linking WhatsApp",
                    "I understand that WhatsApp may restrict or ban this account",
                )
                .into_owned(),
                accept,
                cancel,
                accept_hint,
                restart_note,
            });
        }
        if self.kind == NetworkKind::Telegram {
            // The plan's three paragraphs as they are (the descriptor holds the same words as
            // the English fallback); the tick is WhatsApp's form (D-4 Q1).
            return Some(RiskView {
                title: trc(
                    "@title:window warning before adding a Telegram account",
                    "Before You Add Telegram",
                )
                .into_owned(),
                paragraphs: vec![
                    trc(
                        "@info warning before linking Telegram",
                        "Kourier is not made by Telegram. It uses the Telegram API as an independent app with its own API key.",
                    )
                    .into_owned(),
                    trc(
                        "@info warning before linking Telegram",
                        "Telegram watches accounts used with new apps more closely; mass actions or unusual use can get an account limited or banned.",
                    )
                    .into_owned(),
                    trc(
                        "@info warning before linking Telegram",
                        "Kourier always sends read receipts, typing and online status for Telegram, as Telegram's rules require, and shows the sponsored messages Telegram puts in channels.",
                    )
                    .into_owned(),
                ],
                confirm: trc(
                    "@option:check the person understands the risk of linking Telegram",
                    "I understand that Telegram may limit or ban this account",
                )
                .into_owned(),
                accept,
                cancel,
                accept_hint,
                restart_note,
            });
        }
        // A network the viewmodel has no words for: the descriptor's title and body, and a
        // box that says only that they were read.
        let (title, body) = self.risk_notice.clone().unwrap_or_default();
        Some(RiskView {
            title: single_line(&title),
            paragraphs: body
                .split("\n\n")
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect(),
            confirm: trc(
                "@option:check the person has read the warning before linking",
                "I have read this",
            )
            .into_owned(),
            accept,
            cancel,
            accept_hint,
            restart_note,
        })
    }
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
        assert_eq!(
            network_choices(&[NetworkEntry::plain(NetworkKind::Signal)], false)
                .iter()
                .filter(|c| c.available)
                .count(),
            1
        );
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

    /// An experimental network the way kourier-app tells it: WhatsApp played by the fake, with
    /// both flows and the descriptor's own warning.
    fn whatsapp(on: bool) -> NetworkEntry {
        NetworkEntry {
            kind: NetworkKind::WhatsApp,
            flows: vec![
                ("qr".into(), "QR code".into()),
                ("pair-code".into(), "Phone number".into()),
            ],
            experimental: true,
            risk_notice: Some(("Descriptor title".into(), "Descriptor body".into())),
            history_transfer: false,
            history: HistorySource::PushedOnLink,
            on,
            receipts_always_sent: false,
            missing: None,
        }
    }

    /// Threema the way kourier-app tells it: experimental, no history source (the phone sends
    /// no history to a new device).
    fn threema(on: bool) -> NetworkEntry {
        NetworkEntry {
            kind: NetworkKind::Threema,
            flows: vec![("qr".into(), "Link with a QR code".into())],
            experimental: true,
            risk_notice: Some(("Descriptor title".into(), "Descriptor body".into())),
            history_transfer: false,
            history: HistorySource::None,
            on,
            receipts_always_sent: false,
            missing: None,
        }
    }

    fn kinds(rows: &[NetworkChoice]) -> Vec<NetworkKind> {
        rows.iter().map(|r| r.kind).collect()
    }

    /// The list comes from what the build holds (K-A): Signal first, the test network second,
    /// WhatsApp always once it is in the build, and no "Coming later" row for anything else.
    #[test]
    fn the_list_is_what_the_build_holds() {
        let signal = NetworkEntry::plain(NetworkKind::Signal);
        let fake = NetworkEntry {
            history_transfer: true,
            ..NetworkEntry::plain(NetworkKind::Fake)
        };
        // Whatever order the application lists them in.
        let rows = network_choices(&[whatsapp(false), fake.clone(), signal.clone()], false);
        assert_eq!(
            kinds(&rows),
            [
                NetworkKind::Signal,
                NetworkKind::Fake,
                NetworkKind::WhatsApp
            ]
        );
        assert!(
            rows.iter().all(|r| r.name != "Threema"),
            "no Coming-later row"
        );
        assert!(!rows.iter().any(|r| r.description == "Coming later"));
        assert!(rows[1].history_transfer && !rows[0].history_transfer);
        // Only Signal: one row, as a build without the others.
        assert_eq!(
            kinds(&network_choices(&[signal], false)),
            [NetworkKind::Signal]
        );
    }

    /// U7: the footnote under the list promises that the phone "can send a copy" of its history
    /// only when a network the person can choose NOW (an enabled row) has a history source that
    /// the phone pushes at the link (`PushedOnLink`; KT-1b: not `OnDemand`). A
    /// list of Threema alone (no source) ends at "keeps your message history."; Signal (the offer
    /// on) or WhatsApp beside it brings the long sentence; a row that cannot be chosen
    /// (`available` false: not in the build) promises nothing; an unset source (the offer switched
    /// off) promises nothing. Mutant: the choice always picks the long sentence.
    #[test]
    fn the_footnote_promises_a_copy_only_where_a_row_that_can_be_chosen_has_a_source() {
        const LONG: &str = "Kourier links as an additional device: your phone stays the main device, keeps your message history and can send a copy of it.";
        const SHORT: &str = "Kourier links as an additional device: your phone stays the main device and keeps your message history.";
        let note = |entries: &[NetworkEntry]| welcome_footnote(&network_choices(entries, false));
        let signal = |history| NetworkEntry {
            history,
            ..NetworkEntry::plain(NetworkKind::Signal)
        };
        // Threema alone, on or off (an off experimental row can be chosen, it has no source).
        assert_eq!(note(&[threema(true)]), SHORT);
        assert_eq!(note(&[threema(false)]), SHORT);
        // Signal missing from the build (a fixture run) has no row to choose, only Threema's.
        assert_eq!(note(&[threema(true)]), SHORT);
        // Signal with the transfer offered, WhatsApp, and either beside Threema.
        assert_eq!(note(&[signal(HistorySource::PushedOnLink)]), LONG);
        assert_eq!(note(&[signal(HistorySource::None), whatsapp(false)]), LONG);
        assert_eq!(
            note(&[signal(HistorySource::None), threema(true), whatsapp(true)]),
            LONG
        );
        // A source that is no push (`OnDemand`: the network serves older messages when they are
        // asked for) sends no copy either (KT-1b): the short sentence.
        assert_eq!(
            note(&[signal(HistorySource::OnDemand), threema(true)]),
            SHORT
        );
        // The offer switched off (`KOURIER_HISTORY_TRANSFER=0`): Signal has no source either.
        assert_eq!(note(&[signal(HistorySource::None)]), SHORT);
        assert_eq!(note(&[signal(HistorySource::None), threema(true)]), SHORT);
        // A network with a source in the build that is neither on nor experimental has no row to
        // choose: it promises nothing.
        let off = NetworkEntry {
            on: false,
            history: HistorySource::PushedOnLink,
            ..NetworkEntry::plain(NetworkKind::Fake)
        };
        let rows = network_choices(&[threema(true), off], false);
        assert!(
            rows.iter()
                .any(|r| !r.available && r.history != HistorySource::None)
        );
        assert_eq!(welcome_footnote(&rows), SHORT);
        // Both sentences go through the translator in the context the old text had.
        struct Marks;
        impl crate::i18n::Translator for Marks {
            fn translate(&self, context: &str, msgid: &str) -> String {
                format!("{context}|{msgid}")
            }
            fn translate_plural(&self, _c: &str, one: &str, _m: &str, _n: u64) -> String {
                one.to_owned()
            }
        }
        let (short, long) = crate::i18n::with_translator(Marks, || {
            (
                welcome_footnote(&network_choices(&[threema(true)], false)),
                welcome_footnote(&network_choices(&[whatsapp(true)], false)),
            )
        });
        assert_eq!(short, format!("@info|{SHORT}"));
        assert_eq!(long, format!("@info|{LONG}"));
    }

    /// An experimental network that is off is listed, can be chosen (the warning page turns it
    /// on), and says so; on, it says what it does and a login can start.
    #[test]
    fn an_experimental_network_is_listed_off_and_on() {
        let signal = NetworkEntry::plain(NetworkKind::Signal);
        let off = network_choices(&[signal.clone(), whatsapp(false)], false);
        let wa = &off[1];
        assert_eq!(wa.description, "Off · experimental");
        assert!(wa.available && !wa.on && wa.experimental && wa.asks_first);
        assert_eq!(
            wa.flows.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(),
            ["qr", "pair-code"]
        );
        let on = network_choices(&[signal, whatsapp(true)], false);
        let wa = &on[1];
        assert_eq!(
            wa.description,
            "Link as a secondary device of the WhatsApp app on your phone (experimental)"
        );
        assert!(wa.available && wa.on && wa.experimental && wa.asks_first);
        // The test network played as WhatsApp is no archive transfer: its descriptor says
        // `HistorySource::PushedOnLink`, the flag that keys the screens is its own.
        assert!(!wa.history_transfer);
    }

    /// Signal missing from the build keeps its row and says why (a fixture run, a build
    /// without it), and cannot be chosen.
    #[test]
    fn signal_missing_keeps_its_row_with_the_reason() {
        for (fixture, text) in [
            (true, "Not available in fixture mode"),
            (false, "Not in this build"),
        ] {
            let rows = network_choices(&[NetworkEntry::plain(NetworkKind::Fake)], fixture);
            assert_eq!(kinds(&rows), [NetworkKind::Signal, NetworkKind::Fake]);
            assert_eq!(rows[0].description, text);
            assert!(!rows[0].available && !rows[0].on && rows[0].flows.is_empty());
        }
    }

    /// The warning (K-A §3.4): five paragraphs that do not promise the label the phone shows,
    /// a box that is never ticked for you, and a button that says what it does: "Continue"
    /// while the network is on, "Turn On and Restart" while it is off, with the reason.
    #[test]
    fn the_whatsapp_warning_has_its_paragraphs_box_and_buttons() {
        let rows = network_choices(
            &[NetworkEntry::plain(NetworkKind::Signal), whatsapp(false)],
            false,
        );
        let off = rows[1].risk_view().expect("asks first");
        assert_eq!(off.title, "Before You Link WhatsApp");
        assert_eq!(off.paragraphs.len(), 5);
        assert!(off.paragraphs[0].starts_with("Kourier is not made or approved by WhatsApp."));
        assert!(off.paragraphs[1].contains("restrict or ban"));
        assert!(off.paragraphs[2].contains("never starts a new chat"));
        assert!(off.paragraphs[3].contains("separate phone number"));
        assert_eq!(
            off.paragraphs[4],
            "Your phone lists this computer as a linked device. No usage statistics are sent to WhatsApp."
        );
        // The exact label is not promised anywhere: it is what WhatsApp makes of the name.
        assert!(!off.paragraphs.iter().any(|p| p.contains("Kourier (")));
        assert_eq!(
            off.confirm,
            "I understand that WhatsApp may restrict or ban this account"
        );
        assert_eq!(off.accept, "Turn On and Restart");
        assert_eq!(off.cancel, "Cancel");
        assert_eq!(off.accept_hint, "Tick the box above to continue.");
        assert!(
            off.restart_note.contains("WhatsApp"),
            "{}",
            off.restart_note
        );
        let on_rows = network_choices(
            &[NetworkEntry::plain(NetworkKind::Signal), whatsapp(true)],
            false,
        );
        let on = on_rows[1].risk_view().expect("asks first");
        assert_eq!(on.accept, "Continue");
        assert!(on.restart_note.is_empty());
        assert_eq!(on.paragraphs, off.paragraphs, "the same words either way");
    }

    /// A network that asks nothing has no warning; one the viewmodel has no words for shows the
    /// descriptor's title and body (paragraphs split at blank lines) and a box that says it was
    /// read; its text is a plain string, never markup.
    #[test]
    fn a_network_without_words_shows_the_descriptors_warning() {
        let rows = network_choices(&[NetworkEntry::plain(NetworkKind::Signal)], false);
        assert!(rows[0].risk_view().is_none());
        let threema = NetworkEntry {
            kind: NetworkKind::Threema,
            risk_notice: Some((
                "A\nwarning".into(),
                "First.\n\nSecond. <b>x</b>\n\n  ".into(),
            )),
            ..NetworkEntry::plain(NetworkKind::Threema)
        };
        let rows = network_choices(&[threema], false);
        let v = rows[1].risk_view().expect("asks first");
        assert_eq!(v.title, "A warning", "one line");
        assert_eq!(v.paragraphs, ["First.", "Second. <b>x</b>"]);
        assert_eq!(v.confirm, "I have read this");
        assert_eq!(v.accept, "Continue");
    }

    fn qr(step_id: &str) -> LoginStep {
        LoginStep {
            step_id: step_id.into(),
            txn: 1,
            kind: LoginStepKind::ShowQr {
                data: Redacted::new("link-secret".to_owned()),
                valid_for: None,
            },
        }
    }

    fn code(step_id: &str, code: &str) -> LoginStep {
        LoginStep {
            step_id: step_id.into(),
            txn: 2,
            kind: LoginStepKind::ShowCode {
                code: Redacted::new(code.to_owned()),
            },
        }
    }

    /// The three numbered lines of Signal's QR step moved here from the page (K-A) with the very
    /// same words: the catalogs (Ukrainian included) keep them.
    #[test]
    fn signal_qr_steps_are_the_words_the_page_always_had() {
        let v = login_view(&qr("qr"), NetworkKind::Signal);
        assert_eq!(
            v.steps,
            [
                "Open Signal on your phone",
                "Go to Settings → Linked devices",
                "Tap “Link new device” and scan this code"
            ]
        );
        assert_eq!(
            v.hint,
            "On your phone, open Signal → Settings → Linked devices → Link new device, and scan this code."
        );
        assert_eq!(v.icon, "signal");
    }

    /// WhatsApp's QR step and its code step have their own instructions, and the code reads as
    /// `XXXX-XXXX` (a code that has its hyphen stays as it is).
    #[test]
    fn whatsapp_qr_and_code_steps() {
        let v = login_view(&qr("qr"), NetworkKind::WhatsApp);
        assert_eq!(
            v.steps,
            [
                "Open WhatsApp on your phone",
                "Tap Menu ⋮ (Android) or Settings (iPhone), then “Linked devices”",
                "Tap “Link a device” and scan this code"
            ]
        );
        assert_eq!(v.icon, "whatsapp");
        let c = login_view(&code("code", "ABCD1234"), NetworkKind::WhatsApp);
        assert_eq!(c.kind, LoginViewKind::Code);
        assert_eq!(c.code, "ABCD-1234");
        assert_eq!(
            c.steps,
            [
                "Open WhatsApp on your phone",
                "Tap Menu ⋮ or Settings, then “Linked devices” and “Link a device”",
                "Tap “Link with phone number instead” and type this code"
            ]
        );
        assert!(
            c.hint
                .starts_with("The code is valid for about three minutes.")
        );
        assert_eq!(
            login_view(&code("code", "ABCD-1234"), NetworkKind::WhatsApp).code,
            "ABCD-1234"
        );
        assert!(!format!("{c:?}").contains("ABCD"), "a code is never logged");
    }

    /// A step of a purpose the viewmodel has no words for (a passkey's QR code, a confirmation
    /// code: W9) gets the generic hint and no steps, never another purpose's instructions.
    #[test]
    fn an_unknown_purpose_gets_the_generic_hint() {
        let v = login_view(&qr("passkey"), NetworkKind::WhatsApp);
        assert!(v.steps.is_empty());
        assert_eq!(v.hint, "Scan this code with the app on your phone.");
        let c = login_view(&code("passkey-code", "ABCD1234"), NetworkKind::WhatsApp);
        assert!(c.steps.is_empty());
        assert_eq!(c.hint, "Enter this code on your phone.");
        // And a network it knows nothing about shows the hint alone on its own QR step.
        assert!(login_view(&qr("qr"), NetworkKind::Fake).steps.is_empty());
    }

    /// The line above the fields and the labels of the fields it knows are the viewmodel's, by the
    /// step's purpose; for another step the backend's label stands and nothing is explained.
    #[test]
    fn input_steps_are_explained_by_purpose() {
        let input = |step_id: &str, field: &str, label: &str| LoginStep {
            step_id: step_id.into(),
            txn: 1,
            kind: LoginStepKind::Input {
                fields: vec![kourier_model::InputField {
                    id: field.into(),
                    label: label.into(),
                    kind: InputKind::Text,
                    default: None,
                    required: true,
                    note: None,
                    code: None,
                }],
            },
        };
        let d = login_view(
            &input("device-name", "device-name", "dn"),
            NetworkKind::Signal,
        );
        assert_eq!(
            d.explain,
            "Your phone shows this name in its list of linked devices."
        );
        assert_eq!(d.fields[0].label, "Device name");
        let p = login_view(&input("phone", "phone", "ph"), NetworkKind::WhatsApp);
        assert!(
            p.explain
                .starts_with("Enter the phone number of your WhatsApp account")
        );
        assert!(p.explain.contains("“Chrome (Linux)”"));
        assert_eq!(p.fields[0].label, "Phone number");
        let other = login_view(&input("x", "nick", "Nickname\n(public)"), NetworkKind::Fake);
        assert!(other.explain.is_empty());
        assert_eq!(other.fields[0].label, "Nickname (public)");
    }
    /// A failed step is worded by the step it failed at: the number typed for the pair-code flow
    /// is refused in ways the person can act on; the same error at another step keeps the general
    /// words.
    #[test]
    fn a_link_refused_for_too_many_devices_says_so() {
        let said = |max| {
            error_text(&BackendError::new(
                kourier_model::ErrorKind::TooManyDevices { max },
                "full",
            ))
        };
        assert_eq!(
            said(Some(4)),
            "This account already has as many linked devices as the network allows (4). Remove one in the linked devices on your phone, then try again."
        );
        assert_eq!(
            said(None),
            "This account already has as many linked devices as the network allows. Remove one in the linked devices on your phone, then try again."
        );
    }

    /// The wrong phone during a run that states the account it means to link (W2c): the same words at
    /// the two steps a phone is at (the QR scan, the code), not "Something went wrong." and not the
    /// advice about a mistyped number; the page still offers Try Again. Another "invalid input" keeps its
    /// own words. The number TYPED at `phone` has its own arm (`the_wrong_number_typed_has_words_about_the_number`).
    #[test]
    fn the_wrong_phone_has_a_cause_of_its_own_at_every_step() {
        let failed = |step: &str, kind| {
            login_view(
                &LoginStep {
                    step_id: step.into(),
                    txn: 0,
                    kind: LoginStepKind::Failed {
                        error: BackendError::new(kind, "wrong phone"),
                    },
                },
                NetworkKind::WhatsApp,
            )
        };
        let words = "This phone belongs to another account than the one this run expects. Kourier did not link it.";
        for step in ["qr", "code"] {
            let v = failed(step, ErrorKind::NotTheExpectedAccount);
            assert_eq!(v.error, words, "at {step}");
            assert!(v.retry, "Try Again at {step}");
        }
        assert_eq!(
            error_text(&BackendError::new(ErrorKind::NotTheExpectedAccount, "x")),
            words
        );
        assert_ne!(failed("qr", ErrorKind::InvalidInput).error, words);
        assert!(
            !words.contains("Nothing was linked"),
            "the old claim is back"
        );
    }

    /// L2 (b): a number typed at the `phone` step that is not the one the run expects is worded about
    /// the NUMBER (a typo is the likely cause; nobody scanned a phone), not as a scanned phone, and not
    /// as the old advice about the number's format; the page still offers Try Again. Mutant: the
    /// `("phone", NotTheExpectedAccount)` arm of `failure_text` removed.
    #[test]
    fn the_wrong_number_typed_has_words_about_the_number() {
        let at_phone = |kind| {
            login_view(
                &LoginStep {
                    step_id: "phone".into(),
                    txn: 0,
                    kind: LoginStepKind::Failed {
                        error: BackendError::new(kind, "wrong number"),
                    },
                },
                NetworkKind::WhatsApp,
            )
        };
        let v = at_phone(ErrorKind::NotTheExpectedAccount);
        assert_eq!(
            v.error,
            "This number is not the one this run expects. Check it, or link the expected phone."
        );
        assert!(v.retry, "Try Again at phone");
        // The format advice stays with the format refusal.
        assert!(
            at_phone(ErrorKind::InvalidInput)
                .error
                .starts_with("Check the phone number.")
        );
    }

    /// KI18n fills a plural's `%1` with its COUNT before this crate fills the rest
    /// (`Translator::translate_plural`): the network of a throttled number must not stand in
    /// `%1`, or the page says "5 is limiting link attempts" (K-A review, U2). The test
    /// translator does what KI18n does; the default one never substitutes and hides the fault.
    #[test]
    fn the_wait_of_a_throttled_number_names_the_network_with_a_translator_that_fills_the_count() {
        use std::time::Duration;
        struct LikeKi18n;
        impl crate::i18n::Translator for LikeKi18n {
            fn translate(&self, _context: &str, msgid: &str) -> String {
                msgid.to_owned()
            }
            fn translate_plural(&self, _c: &str, one: &str, many: &str, n: u64) -> String {
                let form = if n == 1 { one } else { many };
                form.replace("%1", &n.to_string())
            }
        }
        let said = |secs: u64| {
            login_view(
                &LoginStep {
                    step_id: "phone".into(),
                    txn: 3,
                    kind: LoginStepKind::Failed {
                        error: BackendError::new(
                            ErrorKind::RateLimited {
                                retry_after: Some(Duration::from_secs(secs)),
                            },
                            "x",
                        ),
                    },
                },
                NetworkKind::WhatsApp,
            )
            .error
        };
        crate::i18n::with_translator(LikeKi18n, || {
            assert_eq!(
                said(300),
                "WhatsApp is limiting link attempts for this number. Wait 5 minutes before you try again."
            );
            assert_eq!(
                said(60),
                "WhatsApp is limiting link attempts for this number. Wait 1 minute before you try again."
            );
            assert_eq!(
                said(3 * 3600),
                "WhatsApp is limiting link attempts for this number. Wait 3 hours before you try again."
            );
        });
    }

    #[test]
    fn a_refused_number_has_words_of_its_own() {
        use std::time::Duration;
        let failed = |step_id: &str, kind: ErrorKind| {
            login_view(
                &LoginStep {
                    step_id: step_id.into(),
                    txn: 3,
                    kind: LoginStepKind::Failed {
                        error: BackendError::new(kind, "x"),
                    },
                },
                NetworkKind::WhatsApp,
            )
        };
        let limited = |step_id: &str, secs: Option<u64>| {
            failed(
                step_id,
                ErrorKind::RateLimited {
                    retry_after: secs.map(Duration::from_secs),
                },
            )
        };
        // Throttled: a wait, in whole minutes (rounded up, never zero), then hours.
        assert_eq!(
            limited("phone", Some(300)).error,
            "WhatsApp is limiting link attempts for this number. Wait 5 minutes before you try again."
        );
        assert!(limited("phone", Some(61)).error.contains("Wait 2 minutes"));
        assert!(
            limited("phone", Some(1))
                .error
                .contains("Wait 1 minute before")
        );
        assert!(
            limited("phone", Some(0))
                .error
                .contains("Wait 1 minute before")
        );
        assert!(
            limited("phone", Some(7200))
                .error
                .contains("Wait 2 hours before")
        );
        assert!(
            limited("phone", Some(3 * 3600 + 1))
                .error
                .contains("Wait 4 hours")
        );
        assert_eq!(
            limited("phone", None).error,
            "WhatsApp is limiting link attempts for this number. Wait a while before you try again."
        );
        assert!(
            limited("phone", Some(300)).retry,
            "asking again later is fine"
        );
        // The same error anywhere else is the general sentence.
        for step in ["qr", "code", "done", "x"] {
            assert_eq!(
                limited(step, Some(300)).error,
                "Too many attempts. Wait a moment and try again.",
                "{step}"
            );
        }
        // Not offered to this account yet: no retry (it would fail the same), a text that
        // points to the QR code.
        let u = failed("phone", ErrorKind::Unsupported);
        assert!(!u.retry && u.leave.is_empty());
        assert_eq!(
            u.error,
            "WhatsApp does not offer linking with a phone number to this account yet. Scan a QR code instead."
        );
        assert!(
            failed("qr", ErrorKind::Unsupported).retry,
            "only for the number"
        );
        // A malformed number: say what to check.
        let bad = failed("phone", ErrorKind::InvalidInput);
        assert!(bad.retry && bad.error.starts_with("Check the phone number."));
        assert_eq!(
            failed("code", ErrorKind::InvalidInput).error,
            "Something went wrong."
        );
        // A code that ran out: the general words, and a retry.
        let e = failed("code", ErrorKind::Expired);
        assert!(e.retry && e.error.starts_with("The code expired"));
    }

    /// The button that switches the flow: the viewmodel's words for `qr` and `pair-code`, the
    /// descriptor's label (one line) for another id.
    #[test]
    fn flow_labels_are_the_viewmodels_for_the_two_it_knows() {
        assert_eq!(
            flow_switch_label("pair-code", "Phone number"),
            "Link with Phone Number Instead"
        );
        assert_eq!(flow_switch_label("qr", "QR code"), "Scan a QR Code Instead");
        assert_eq!(
            flow_switch_label("passkey", "Use a\npasskey"),
            "Use a passkey"
        );
    }

    fn emoji_step(glyphs: &[&str]) -> LoginStep {
        LoginStep {
            step_id: "emoji".into(),
            txn: 3,
            kind: LoginStepKind::ConfirmEmoji {
                emoji: glyphs.iter().map(|g| (*g).to_owned()).collect(),
            },
        }
    }

    fn failed_at(step: &str, kind: ErrorKind, network: NetworkKind) -> LoginView {
        login_view(
            &LoginStep {
                step_id: step.into(),
                txn: 4,
                kind: LoginStepKind::Failed {
                    error: BackendError::new(kind, "x"),
                },
            },
            network,
        )
    }

    /// QA (his words): the Emoji step is titled "Compare the Emoji", says where to confirm, and
    /// hands the page three glyphs each with its name; a glyph the table does not know keeps
    /// its place with an empty name. The names go through the translator in their own context.
    #[test]
    fn the_emoji_step_shows_three_glyphs_with_their_names() {
        let monkey = "\u{1F435}";
        let cat = "\u{1F408}\u{200D}\u{2B1B}";
        let sunflower = "\u{1F33B}";
        let v = login_view(&emoji_step(&[monkey, cat, sunflower]), NetworkKind::Threema);
        assert_eq!(v.kind, LoginViewKind::Emoji);
        assert_eq!(v.kind.as_str(), "emoji");
        assert_eq!(v.title, "Compare the Emoji");
        assert_eq!(
            v.hint,
            "Check that your phone shows the same three emoji, then confirm on your phone."
        );
        let shown: Vec<(&str, &str)> = v
            .emoji
            .iter()
            .map(|e| (e.glyph.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (monkey, "Monkey"),
                (cat, "Black Cat"),
                (sunflower, "Sunflower")
            ]
        );
        assert_eq!(v.icon, "threema");
        assert!(v.qr.is_empty() && v.code.is_empty() && v.steps.is_empty() && v.fields.is_empty());
        assert!(!v.rotates, "no code to rotate at the emoji step");
        assert_eq!(v.step_id, "emoji");
        assert_eq!(v.txn, 3);

        // An unknown glyph (and a table glyph without its variation selector: the table is
        // exact) is shown, nameless.
        let odd = login_view(
            &emoji_step(&["\u{1F642}", "\u{1F43F}", "\u{1F43F}\u{FE0F}"]),
            NetworkKind::Threema,
        );
        let odd: Vec<(&str, &str)> = odd
            .emoji
            .iter()
            .map(|e| (e.glyph.as_str(), e.name.as_str()))
            .collect();
        assert_eq!(
            odd,
            [
                ("\u{1F642}", ""),
                ("\u{1F43F}", ""),
                ("\u{1F43F}\u{FE0F}", "Chipmunk")
            ]
        );
        assert_eq!(emoji_name("\u{1F642}"), None);
        assert_eq!(emoji_name(""), None);

        // The names are looked up in a context of their own, through the translator.
        struct Marks;
        impl crate::i18n::Translator for Marks {
            fn translate(&self, context: &str, msgid: &str) -> String {
                format!("{context}|{msgid}")
            }
            fn translate_plural(&self, _c: &str, one: &str, _m: &str, _n: u64) -> String {
                one.to_owned()
            }
        }
        crate::i18n::with_translator(Marks, || {
            assert_eq!(
                emoji_name(sunflower).as_deref(),
                Some("@label name of an emoji the phone shows when linking|Sunflower")
            );
            let v = login_view(&emoji_step(&[monkey]), NetworkKind::Threema);
            assert!(v.emoji[0].name.ends_with("|Monkey"), "{:?}", v.emoji);
            assert!(v.title.ends_with("|Compare the Emoji"));
        });
    }

    /// All 128 emoji of Threema's linking wizard have a name, each its own. THE LIST IS THIS
    /// TEST'S OWN, copied from Threema Desktop (`apps/desktop/src/app/ui/linking/emoji-list.ts`
    /// of threema-desktop `93a6615c`, lines 3-130, one entry per line, in that order; the
    /// entries with a variation selector, such as the chipmunk, keep it) and independent of the
    /// backend's table (`kourier-threema` `join::emoji::EMOJI`, which has a digest test against
    /// the same file). The English names are QB's (short, capitalised, one or two words:
    /// `target/threema/th4-k2a-names.md` lists them with CLDR's).
    #[test]
    fn every_one_of_the_128_emoji_has_its_name() {
        const DESKTOP: [(&str, &str); 128] = [
            ("\u{1F435}", "Monkey"),                     // 🐵
            ("\u{1F436}", "Dog"),                        // 🐶
            ("\u{1F429}", "Poodle"),                     // 🐩
            ("\u{1F98A}", "Fox"),                        // 🦊
            ("\u{1F408}\u{200D}\u{2B1B}", "Black Cat"),  // 🐈‍⬛
            ("\u{1F434}", "Horse"),                      // 🐴
            ("\u{1F9AC}", "Bison"),                      // 🦬
            ("\u{1F42E}", "Cow"),                        // 🐮
            ("\u{1F437}", "Pig"),                        // 🐷
            ("\u{1F42A}", "Camel"),                      // 🐪
            ("\u{1F992}", "Giraffe"),                    // 🦒
            ("\u{1F418}", "Elephant"),                   // 🐘
            ("\u{1F42D}", "Mouse"),                      // 🐭
            ("\u{1F430}", "Rabbit"),                     // 🐰
            ("\u{1F43F}\u{FE0F}", "Chipmunk"),           // 🐿️
            ("\u{1F43C}", "Panda"),                      // 🐼
            ("\u{1F998}", "Kangaroo"),                   // 🦘
            ("\u{1F414}", "Chicken"),                    // 🐔
            ("\u{1F54A}\u{FE0F}", "Dove"),               // 🕊️
            ("\u{1F986}", "Duck"),                       // 🦆
            ("\u{1F9A2}", "Swan"),                       // 🦢
            ("\u{1F989}", "Owl"),                        // 🦉
            ("\u{1F9A4}", "Dodo"),                       // 🦤
            ("\u{1FAB6}", "Feather"),                    // 🪶
            ("\u{1F9A9}", "Flamingo"),                   // 🦩
            ("\u{1F438}", "Frog"),                       // 🐸
            ("\u{1F40B}", "Whale"),                      // 🐋
            ("\u{1F42C}", "Dolphin"),                    // 🐬
            ("\u{1F9AD}", "Seal"),                       // 🦭
            ("\u{1F41F}", "Fish"),                       // 🐟
            ("\u{1F41A}", "Spiral Shell"),               // 🐚
            ("\u{1FAB2}", "Beetle"),                     // 🪲
            ("\u{1F578}\u{FE0F}", "Spider Web"),         // 🕸️
            ("\u{1F3F5}\u{FE0F}", "Rosette"),            // 🏵️
            ("\u{1F33B}", "Sunflower"),                  // 🌻
            ("\u{1F332}", "Evergreen Tree"),             // 🌲
            ("\u{1F33F}", "Herb"),                       // 🌿
            ("\u{2618}\u{FE0F}", "Shamrock"),            // ☘️
            ("\u{1F349}", "Watermelon"),                 // 🍉
            ("\u{1F34B}", "Lemon"),                      // 🍋
            ("\u{1F34E}", "Red Apple"),                  // 🍎
            ("\u{1F352}", "Cherries"),                   // 🍒
            ("\u{1F353}", "Strawberry"),                 // 🍓
            ("\u{1FAD0}", "Blueberries"),                // 🫐
            ("\u{1F345}", "Tomato"),                     // 🍅
            ("\u{1F951}", "Avocado"),                    // 🥑
            ("\u{1F955}", "Carrot"),                     // 🥕
            ("\u{1F966}", "Broccoli"),                   // 🥦
            ("\u{1F344}", "Mushroom"),                   // 🍄
            ("\u{1FAD8}", "Beans"),                      // 🫘
            ("\u{1F9C2}", "Salt"),                       // 🧂
            ("\u{1F36A}", "Cookie"),                     // 🍪
            ("\u{1F36B}", "Chocolate Bar"),              // 🍫
            ("\u{2615}", "Hot Beverage"),                // ☕
            ("\u{1F9CA}", "Ice Cube"),                   // 🧊
            ("\u{1F962}", "Chopsticks"),                 // 🥢
            ("\u{1F5FA}\u{FE0F}", "World Map"),          // 🗺️
            ("\u{1F30B}", "Volcano"),                    // 🌋
            ("\u{1F3D5}\u{FE0F}", "Camping"),            // 🏕️
            ("\u{1F3DD}\u{FE0F}", "Desert Island"),      // 🏝️
            ("\u{1F3DB}\u{FE0F}", "Classical Building"), // 🏛️
            ("\u{1F682}", "Locomotive"),                 // 🚂
            ("\u{1F69A}", "Delivery Truck"),             // 🚚
            ("\u{1F69C}", "Tractor"),                    // 🚜
            ("\u{1F6E4}\u{FE0F}", "Railway Track"),      // 🛤️
            ("\u{2693}", "Anchor"),                      // ⚓
            ("\u{1F6F0}\u{FE0F}", "Satellite"),          // 🛰️
            ("\u{1F680}", "Rocket"),                     // 🚀
            ("\u{1F319}", "Crescent Moon"),              // 🌙
            ("\u{2600}\u{FE0F}", "Sun"),                 // ☀️
            ("\u{2B50}", "Star"),                        // ⭐
            ("\u{1F308}", "Rainbow"),                    // 🌈
            ("\u{2602}\u{FE0F}", "Umbrella"),            // ☂️
            ("\u{2744}\u{FE0F}", "Snowflake"),           // ❄️
            ("\u{2603}\u{FE0F}", "Snowman"),             // ☃️
            ("\u{1F525}", "Fire"),                       // 🔥
            ("\u{1F4A7}", "Droplet"),                    // 💧
            ("\u{2728}", "Sparkles"),                    // ✨
            ("\u{1F388}", "Balloon"),                    // 🎈
            ("\u{1F380}", "Ribbon"),                     // 🎀
            ("\u{1F947}", "Gold Medal"),                 // 🥇
            ("\u{1F3C0}", "Basketball"),                 // 🏀
            ("\u{1F3D0}", "Volleyball"),                 // 🏐
            ("\u{1F3B3}", "Bowling"),                    // 🎳
            ("\u{1F3D3}", "Ping Pong"),                  // 🏓
            ("\u{26F3}", "Golf Flag"),                   // ⛳
            ("\u{1F3AF}", "Bullseye"),                   // 🎯
            ("\u{1F579}\u{FE0F}", "Joystick"),           // 🕹️
            ("\u{1F9E9}", "Puzzle Piece"),               // 🧩
            ("\u{1F9F8}", "Teddy Bear"),                 // 🧸
            ("\u{2660}\u{FE0F}", "Spade Suit"),          // ♠️
            ("\u{2665}\u{FE0F}", "Heart Suit"),          // ♥️
            ("\u{1F457}", "Dress"),                      // 👗
            ("\u{1F451}", "Crown"),                      // 👑
            ("\u{1F514}", "Bell"),                       // 🔔
            ("\u{1F3B7}", "Saxophone"),                  // 🎷
            ("\u{1F3B8}", "Guitar"),                     // 🎸
            ("\u{1F5A8}\u{FE0F}", "Printer"),            // 🖨️
            ("\u{1F4F8}", "Camera"),                     // 📸
            ("\u{1F56F}\u{FE0F}", "Candle"),             // 🕯️
            ("\u{1F4D6}", "Open Book"),                  // 📖
            ("\u{1F4E6}", "Package"),                    // 📦
            ("\u{1F4EE}", "Postbox"),                    // 📮
            ("\u{1F4DD}", "Memo"),                       // 📝
            ("\u{1F4BC}", "Briefcase"),                  // 💼
            ("\u{1F4CB}", "Clipboard"),                  // 📋
            ("\u{1F512}", "Lock"),                       // 🔒
            ("\u{1F511}", "Key"),                        // 🔑
            ("\u{2692}\u{FE0F}", "Hammer and Pick"),     // ⚒️
            ("\u{1FA83}", "Boomerang"),                  // 🪃
            ("\u{2696}\u{FE0F}", "Balance Scale"),       // ⚖️
            ("\u{1F517}", "Chain Link"),                 // 🔗
            ("\u{1FA9D}", "Hook"),                       // 🪝
            ("\u{1F52C}", "Microscope"),                 // 🔬
            ("\u{1FA91}", "Chair"),                      // 🪑
            ("\u{1F6BD}", "Toilet"),                     // 🚽
            ("\u{1F9F9}", "Broom"),                      // 🧹
            ("\u{1FAA3}", "Bucket"),                     // 🪣
            ("\u{1FAE7}", "Bubbles"),                    // 🫧
            ("\u{26AB}", "Black Circle"),                // ⚫
            ("\u{1F7E8}", "Yellow Square"),              // 🟨
            ("\u{25B6}\u{FE0F}", "Play Button"),         // ▶️
            ("\u{1F4F6}", "Antenna Bars"),               // 📶
            ("\u{1F4A5}", "Collision"),                  // 💥
            ("\u{1F4AC}", "Speech Balloon"),             // 💬
            ("\u{1F4AB}", "Dizzy"),                      // 💫
            ("\u{1F440}", "Eyes"),                       // 👀
            ("\u{1F463}", "Footprints"),                 // 👣
        ];
        let mut glyphs = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for (index, (glyph, name)) in DESKTOP.iter().enumerate() {
            assert_eq!(
                emoji_name(glyph).as_deref(),
                Some(*name),
                "entry {index} (emoji-list.ts:{})",
                index + 3
            );
            assert!(glyphs.insert(*glyph), "{index}: the glyph is listed twice");
            assert!(names.insert(*name), "{index}: {name:?} names two emoji");
            // QB: capitalised, short. Three words only with a small word between ("Hammer and
            // Pick"), never a sentence; no punctuation, no trailing space.
            let words: Vec<&str> = name.split(' ').collect();
            assert!(
                (1..=3).contains(&words.len()) && name.trim() == *name,
                "{name:?}"
            );
            assert!(
                words
                    .iter()
                    .all(|w| !w.is_empty() && w.chars().all(char::is_alphanumeric)),
                "{name:?}"
            );
            assert!(
                words
                    .iter()
                    .all(|w| *w == "and" || w.chars().next().is_some_and(char::is_uppercase)),
                "Title Case: {name:?}"
            );
        }
        assert_eq!((glyphs.len(), names.len()), (128, 128));
    }

    /// The page says "the code changes every few seconds" for every QR code but Threema's, which
    /// is ONE code for the whole link (QD), whether or not the step names a time: Telegram's
    /// token rotates and names none, the fake that plays Threema names one. Another step kind has
    /// no code.
    #[test]
    fn every_qr_code_rotates_but_threemas() {
        use std::time::Duration;
        let with = |network, valid_for| {
            login_view(
                &LoginStep {
                    step_id: "qr".into(),
                    txn: 1,
                    kind: LoginStepKind::ShowQr {
                        data: Redacted::new("link-secret".to_owned()),
                        valid_for,
                    },
                },
                network,
            )
        };
        let minute = Some(Duration::from_secs(60));
        for network in [
            NetworkKind::Signal,
            NetworkKind::WhatsApp,
            NetworkKind::Telegram,
            NetworkKind::Fake,
        ] {
            assert!(with(network, minute).rotates, "{network:?} with a time");
            assert!(with(network, None).rotates, "{network:?} without a time");
        }
        assert!(!with(NetworkKind::Threema, None).rotates, "the real actor");
        assert!(
            !with(NetworkKind::Threema, minute).rotates,
            "the fake playing Threema"
        );
        for network in [
            NetworkKind::Signal,
            NetworkKind::WhatsApp,
            NetworkKind::Threema,
        ] {
            assert!(
                !login_view(&code("code", "ABCD1234"), network).rotates,
                "{network:?}: a code to type"
            );
        }
        assert!(!LoginView::default().rotates);
    }

    /// QD (his words): Threema's QR step has three numbered steps and no line about the code
    /// changing; the hint is the generic one; only the `qr` purpose has steps.
    #[test]
    fn threemas_qr_step_has_three_numbered_steps() {
        let v = login_view(&qr("qr"), NetworkKind::Threema);
        assert_eq!(v.kind, LoginViewKind::Qr);
        assert_eq!(
            v.steps,
            [
                "Open Threema on your phone",
                "Go to Settings → Linked Devices",
                "Tap the button to link a device and scan this code"
            ]
        );
        assert_eq!(v.title, "Link Threema");
        assert_eq!(v.hint, "Scan this code with the app on your phone.");
        assert_eq!(v.icon, "threema");
        assert!(!v.rotates, "the code does not change: no line about it");
        assert!(
            login_view(&qr("passkey"), NetworkKind::Threema)
                .steps
                .is_empty()
        );
        // Signal's and WhatsApp's words are not Threema's.
        assert_ne!(v.steps, login_view(&qr("qr"), NetworkKind::Signal).steps);
    }

    /// QC (his words) with E2: Threema's two refusals are the same kind (`NotTheExpectedAccount`),
    /// told apart by the step. At `qr` the profile had no test ID (no code was shown), at
    /// `linking` the phone's ID was not on the list. Another step, another kind or another network
    /// keeps the general words.
    #[test]
    fn threemas_two_refusals_have_their_own_sentences() {
        use ErrorKind::NotTheExpectedAccount;
        let none = "Kourier links Threema only in a prepared test profile. No code was shown.";
        let unlisted =
            "This Threema ID is not on Kourier's list of test IDs. Kourier did not link it.";
        assert_eq!(
            failed_at("qr", NotTheExpectedAccount, NetworkKind::Threema).error,
            none
        );
        assert_eq!(
            failed_at("linking", NotTheExpectedAccount, NetworkKind::Threema).error,
            unlisted
        );
        for step in ["qr", "linking"] {
            let v = failed_at(step, NotTheExpectedAccount, NetworkKind::Threema);
            assert_eq!(v.kind, LoginViewKind::Failed);
            assert!(!v.error.is_empty() && v.error.ends_with('.'));
        }
        // The general words are for every other case.
        let general = error_text(&BackendError::new(NotTheExpectedAccount, "x"));
        assert_eq!(
            failed_at("emoji", NotTheExpectedAccount, NetworkKind::Threema).error,
            general
        );
        assert_eq!(
            failed_at("qr", NotTheExpectedAccount, NetworkKind::WhatsApp).error,
            general
        );
        assert_eq!(
            failed_at("linking", NotTheExpectedAccount, NetworkKind::WhatsApp).error,
            general
        );
        assert_eq!(
            failed_at("linking", ErrorKind::Unsupported, NetworkKind::Threema).error,
            "Something went wrong."
        );
        assert_eq!(
            failed_at("qr", ErrorKind::Expired, NetworkKind::Threema).error,
            "The code expired. Try again for a new one."
        );
    }

    /// U3: Threema's first refusal (`qr`: the profile names no test ID, B1's gate refuses before
    /// any socket) fails the same at every new attempt until Kourier restarts, so it offers no Try
    /// Again but the way back ("OK", the form of an account that is here already). The second
    /// refusal (`linking`: this phone's ID is not on the list) keeps Try Again, since another phone
    /// can pass; the same kind at `qr` for WhatsApp, or at another Threema step, keeps it too.
    /// Mutant: the Threema arm of the `Failed` match removed.
    #[test]
    fn threemas_first_refusal_offers_no_try_again_and_the_second_does() {
        use ErrorKind::NotTheExpectedAccount;
        let gate = failed_at("qr", NotTheExpectedAccount, NetworkKind::Threema);
        assert_eq!(gate.kind, LoginViewKind::Failed);
        assert!(!gate.retry, "no Try Again for a gate that refuses again");
        assert_eq!(gate.leave, "OK");
        // The sentence is QC's and does not change with the button.
        assert_eq!(
            gate.error,
            "Kourier links Threema only in a prepared test profile. No code was shown."
        );
        let unlisted = failed_at("linking", NotTheExpectedAccount, NetworkKind::Threema);
        assert!(unlisted.retry, "Try Again for another phone");
        assert!(unlisted.leave.is_empty());
        for (step, network) in [
            ("qr", NetworkKind::WhatsApp),
            ("qr", NetworkKind::Signal),
            ("emoji", NetworkKind::Threema),
        ] {
            let v = failed_at(step, NotTheExpectedAccount, network);
            assert!(v.retry && v.leave.is_empty(), "{step} {network:?}");
        }
        // The other kinds at Threema's `qr` step keep Try Again.
        let expired = failed_at("qr", ErrorKind::Expired, NetworkKind::Threema);
        assert!(expired.retry && expired.leave.is_empty());
    }

    // ---------- Telegram's words (KT-1b) ----------

    /// Telegram the way kourier-app tells it: experimental, QR first, then the phone number, its
    /// history served on demand, its accounts always sending receipts.
    fn telegram(on: bool) -> NetworkEntry {
        NetworkEntry {
            kind: NetworkKind::Telegram,
            flows: vec![
                ("qr".into(), "Scan a QR Code".into()),
                ("phone".into(), "Phone Number".into()),
            ],
            experimental: true,
            risk_notice: Some(("Descriptor title".into(), "Descriptor body".into())),
            history_transfer: false,
            history: HistorySource::OnDemand,
            on,
            receipts_always_sent: true,
            missing: None,
        }
    }

    fn tg_field(
        id: &str,
        kind: InputKind,
        note: Option<&str>,
        code: Option<kourier_model::CodeDelivery>,
    ) -> InputField {
        InputField {
            id: id.into(),
            label: format!("Backend label of {id}"),
            kind,
            default: None,
            required: true,
            note: note.map(|n| kourier_model::NetworkNote {
                explanation: Some(n.to_owned()),
                appeal_url: None,
            }),
            code,
        }
    }

    fn tg_input(step_id: &str, fields: Vec<InputField>) -> LoginStep {
        LoginStep {
            step_id: step_id.into(),
            txn: 2,
            kind: LoginStepKind::Input { fields },
        }
    }

    fn delivery(
        via: CodeVia,
        resend: Option<(u64, CodeVia)>,
    ) -> Option<kourier_model::CodeDelivery> {
        Some(kourier_model::CodeDelivery {
            via,
            resend: resend.map(|(secs, via)| kourier_model::CodeResend {
                after: Duration::from_secs(secs),
                via,
            }),
        })
    }

    /// The warning has Telegram's title, the plan's three paragraphs and its own tick, with the
    /// buttons every network's warning has: "Continue" while it is on, "Turn On and Restart" while
    /// it is off. Mutant: the Telegram arm of `risk_view` removed (the tick falls back to "I have
    /// read this" and the title to the descriptor's).
    #[test]
    fn telegrams_warning_has_its_own_words_and_tick() {
        let rows = |on| {
            network_choices(
                &[NetworkEntry::plain(NetworkKind::Signal), telegram(on)],
                false,
            )
        };
        let off = rows(false)[1].risk_view().expect("asks first");
        assert_eq!(off.title, "Before You Add Telegram");
        assert_eq!(
            off.paragraphs,
            [
                "Kourier is not made by Telegram. It uses the Telegram API as an independent app with its own API key.",
                "Telegram watches accounts used with new apps more closely; mass actions or unusual use can get an account limited or banned.",
                "Kourier always sends read receipts, typing and online status for Telegram, as Telegram's rules require, and shows the sponsored messages Telegram puts in channels."
            ]
        );
        assert_eq!(
            off.confirm,
            "I understand that Telegram may limit or ban this account"
        );
        assert_eq!(off.accept, "Turn On and Restart");
        assert_eq!(off.cancel, "Cancel");
        assert_eq!(off.accept_hint, "Tick the box above to continue.");
        assert!(
            off.restart_note.contains("Telegram"),
            "{}",
            off.restart_note
        );
        let on = rows(true)[1].risk_view().expect("asks first");
        assert_eq!(on.accept, "Continue");
        assert!(on.restart_note.is_empty());
        assert_eq!((on.title, on.paragraphs), (off.title, off.paragraphs));
    }

    /// Telegram's QR step has three numbered lines; the code rotates and the hint is the generic one.
    /// Mutant: the Telegram arm of `qr_steps` removed.
    #[test]
    fn telegrams_qr_step_has_three_numbered_steps() {
        let v = login_view(&qr("qr"), NetworkKind::Telegram);
        assert_eq!(
            v.steps,
            [
                "Open Telegram on your phone",
                "Go to Settings → Devices",
                "Tap “Link Desktop Device” and scan this code"
            ]
        );
        assert!(v.rotates, "Telegram's token rotates");
        assert_eq!(v.hint, "Scan this code with the app on your phone.");
        assert_eq!(v.icon, "telegram");
        assert_eq!(v.title, "Link Telegram");
        // A step of another purpose gets none.
        assert!(
            login_view(&qr("passkey"), NetworkKind::Telegram)
                .steps
                .is_empty()
        );
    }

    /// The line above the fields is keyed on the network AND the step: Telegram's `phone` names
    /// Telegram and says nothing of "Chrome (Linux)", WhatsApp's keeps its two sentences; the
    /// code step says where the code went, by the typed way; the four retry ids say the last entry
    /// was not right; the password points to the app; the e-mail steps say what they ask.
    /// Mutants: `input_explain` keyed on the step only (Telegram's phone shows WhatsApp's words);
    /// the SMS sentence replaced by the app's.
    #[test]
    fn input_steps_are_explained_by_network_and_step() {
        let explain = |network, step: &str, fields: Vec<InputField>| {
            login_view(&tg_input(step, fields), network).explain
        };
        let phone = || vec![tg_field("phone", InputKind::Phone, None, None)];
        let tg = explain(NetworkKind::Telegram, "phone", phone());
        assert_eq!(
            tg,
            "Enter the phone number of your Telegram account with its country code, for example +380 67 123 4567."
        );
        assert!(!tg.contains("Chrome"), "{tg}");
        let wa = explain(NetworkKind::WhatsApp, "phone", phone());
        assert!(wa.starts_with("Enter the phone number of your WhatsApp account"));
        assert!(wa.contains("“Chrome (Linux)”"));
        // Every `CodeVia` has its own sentence.
        let said = |via: Option<CodeVia>| {
            explain(
                NetworkKind::Telegram,
                "code",
                vec![tg_field(
                    "code",
                    InputKind::Text,
                    None,
                    via.and_then(|via| delivery(via, None)),
                )],
            )
        };
        let all = [
            (
                CodeVia::App,
                "Telegram sent the code to your Telegram app on another device.",
            ),
            (CodeVia::Sms, "Telegram sent the code by SMS."),
            (
                CodeVia::SmsWord,
                "Telegram sent a word by SMS. Enter that word.",
            ),
            (
                CodeVia::SmsPhrase,
                "Telegram sent a phrase by SMS. Enter that phrase.",
            ),
            (
                CodeVia::Call,
                "Telegram is calling you to tell you the code.",
            ),
            (
                CodeVia::Fragment,
                "Telegram sent the code to your anonymous number on Fragment.",
            ),
            (CodeVia::Other, "Enter the code Telegram sent you."),
        ];
        for (via, text) in all {
            assert_eq!(said(Some(via)), text, "{via:?}");
        }
        assert_eq!(
            said(None),
            "Enter the code Telegram sent you.",
            "no delivery"
        );
        // The retry ids and the other Telegram steps.
        let wrong_code = "That code is not right. Check it and enter it again.";
        for step in ["code-again", "email-code-again"] {
            assert_eq!(
                explain(
                    NetworkKind::Telegram,
                    step,
                    vec![tg_field(
                        "code",
                        InputKind::Text,
                        None,
                        delivery(CodeVia::Sms, None)
                    )]
                ),
                wrong_code,
                "{step}"
            );
        }
        let secret = || vec![tg_field("password", InputKind::Secret, None, None)];
        assert_eq!(
            explain(NetworkKind::Telegram, "password", secret()),
            "This account has two-step verification. Enter its password.\n\nForgot it? Reset it in the Telegram app: Kourier cannot."
        );
        assert_eq!(
            explain(NetworkKind::Telegram, "password-again", secret()),
            "That password is not right. Try again.\n\nForgot it? Reset it in the Telegram app: Kourier cannot."
        );
        let email = || vec![tg_field("email", InputKind::Text, None, None)];
        assert_eq!(
            explain(NetworkKind::Telegram, "email", email()),
            "Telegram asks for an email address for this account's logins. It sends a code there next."
        );
        assert_eq!(
            explain(NetworkKind::Telegram, "email-again", email()),
            "Telegram does not accept that address. Check it and try again."
        );
        assert_eq!(
            explain(
                NetworkKind::Telegram,
                "email-code",
                vec![tg_field("code", InputKind::Text, None, None)]
            ),
            "Enter the code Telegram sent to your email address."
        );
        // A step of another purpose is not explained; Signal's device name still is.
        assert!(explain(NetworkKind::Telegram, "linking", phone()).is_empty());
        assert!(
            explain(
                NetworkKind::Signal,
                "device-name",
                vec![tg_field("device-name", InputKind::Text, None, None)]
            )
            .starts_with("Your phone shows this name")
        );
    }

    /// The labels of Telegram's fields are the viewmodel's (Code, Word, Phrase, Password, Email
    /// address), and a field's note comes out as ONE plain line in the viewmodel's words: the
    /// password's hint after "Hint:", the first letter or word of an SMS code after "It starts with".
    /// A note of a field without words for it, an empty one, and a hint with a newline are handled;
    /// a placeholder in the network's text is not filled in. Mutants: the note is not copied;
    /// the hint keeps its newline.
    #[test]
    fn the_field_labels_and_notes_are_the_viewmodels() {
        let view = |step: &str, fields: Vec<InputField>| {
            login_view(&tg_input(step, fields), NetworkKind::Telegram)
        };
        let v = view(
            "password",
            vec![tg_field(
                "password",
                InputKind::Secret,
                Some("my first\nPET %1 and %2\t"),
                None,
            )],
        );
        assert_eq!(v.fields[0].label, "Password");
        assert_eq!(v.fields[0].kind, "secret");
        assert_eq!(
            v.fields[0].note, "Hint: my first PET %1 and %2",
            "one line, the network's text is not filled in"
        );
        // A hint that is only whitespace is no hint.
        let blank = view(
            "password",
            vec![tg_field("password", InputKind::Secret, Some(" \n "), None)],
        );
        assert!(blank.fields[0].note.is_empty());
        // A long hint is cut like a banner's explanation.
        let long = "h".repeat(500);
        let cut = view(
            "password",
            vec![tg_field("password", InputKind::Secret, Some(&long), None)],
        );
        assert_eq!(cut.fields[0].note.chars().count(), "Hint: ".len() + 400 + 1);
        assert!(cut.fields[0].note.ends_with('…'));
        // The code field: its label by the way it came, its note only for a word or a phrase.
        let code = |via, note| {
            view(
                "code",
                vec![tg_field("code", InputKind::Text, note, delivery(via, None))],
            )
        };
        assert_eq!(code(CodeVia::App, None).fields[0].label, "Code");
        assert_eq!(code(CodeVia::Sms, None).fields[0].label, "Code");
        let word = code(CodeVia::SmsWord, Some("B"));
        assert_eq!(word.fields[0].label, "Word");
        assert_eq!(word.fields[0].note, "It starts with “B”.");
        let phrase = code(CodeVia::SmsPhrase, Some("blue\nsky"));
        assert_eq!(phrase.fields[0].label, "Phrase");
        assert_eq!(phrase.fields[0].note, "It starts with “blue sky”.");
        assert!(
            code(CodeVia::Sms, Some("ignored")).fields[0]
                .note
                .is_empty(),
            "a note on a field without words for it is not shown"
        );
        let email = view(
            "email",
            vec![tg_field("email", InputKind::Text, None, None)],
        );
        assert_eq!(email.fields[0].label, "Email address");
        assert!(email.fields[0].note.is_empty());
        // The e-mail code field is a "Code" too (its id is `code`).
        assert_eq!(
            view(
                "email-code",
                vec![tg_field("code", InputKind::Text, None, None)]
            )
            .fields[0]
                .label,
            "Code"
        );
        // The foreign text is never logged.
        assert!(!format!("{:?}", v.fields[0]).contains("PET"));
    }

    /// "Send a New Code" is offered only where the network allows another code (the HIG: an action
    /// that does nothing is not shown), with the wait that REMAINS when the step is shown (0 = at
    /// once); no delivery, or one without a next way, offers none. Mutants: the label is set with
    /// `resend: None`; the button only at the first `code` step (F4, `code-again` has it too).
    #[test]
    fn a_new_code_is_offered_only_when_the_network_allows_it() {
        let code = |d| {
            login_view(
                &tg_input("code", vec![tg_field("code", InputKind::Text, None, d)]),
                NetworkKind::Telegram,
            )
        };
        let v = code(delivery(CodeVia::App, Some((25, CodeVia::Sms))));
        assert_eq!(
            (v.resend.as_str(), v.resend_after_ms),
            ("Send a New Code", 25_000)
        );
        let now = code(delivery(CodeVia::App, Some((0, CodeVia::Sms))));
        assert_eq!(
            (now.resend.as_str(), now.resend_after_ms),
            ("Send a New Code", 0)
        );
        for none in [delivery(CodeVia::Sms, None), None] {
            let v = code(none);
            assert!(v.resend.is_empty() && v.resend_after_ms == 0);
        }
        // The retry step after a wrong code states the delivery too (the backend's `input_step`),
        // and the wait that remains: the button is there at `code-again` as at `code` (fix round
        // F4). Mutant: the button only at the first `code` step.
        let again = login_view(
            &tg_input(
                "code-again",
                vec![tg_field(
                    "code",
                    InputKind::Text,
                    None,
                    delivery(CodeVia::Sms, Some((17, CodeVia::Call))),
                )],
            ),
            NetworkKind::Telegram,
        );
        assert_eq!(
            (again.resend.as_str(), again.resend_after_ms),
            ("Send a New Code", 17_000),
            "code-again"
        );
        // Another step has no code field, so no button.
        let pw = login_view(
            &tg_input(
                "password",
                vec![tg_field("password", InputKind::Secret, None, None)],
            ),
            NetworkKind::Telegram,
        );
        assert!(pw.resend.is_empty());
        // The e-mail code is a code step without a delivery: no button.
        let ec = login_view(
            &tg_input(
                "email-code",
                vec![tg_field("code", InputKind::Text, None, None)],
            ),
            NetworkKind::Telegram,
        );
        assert!(ec.resend.is_empty());
    }

    /// Telegram's refusals have their own sentences, by the step they happened at; the failure-only
    /// ids `registration`, `purchase` and `update` need no network guard, the pairs that other
    /// networks use too (`(phone, Unauthorized)`, `(code, Unsupported)`) do. A banned number, a
    /// number without an account and a newer Kourier asked for offer no Try Again (asking again
    /// fails the same) and go back with "OK"; a purchase asked for before a code is sent offers no
    /// Try Again either but NO leave button, so the QR flow, which sends no code, stays (fix round
    /// F2); a code Kourier cannot take, and an e-mail address Telegram does not accept, keep Try
    /// Again. Mutants: drop the Telegram guard on `(code, Unsupported)` (WhatsApp's control
    /// fails), drop the `registration` arm (generic words), drop the Telegram guard on
    /// `(phone, Unauthorized)`, give `purchase` the "OK" arm again.
    #[test]
    fn telegrams_refusals_have_their_own_sentences() {
        use ErrorKind::{RateLimited, Unauthorized, Unsupported};
        let tg = |step, kind| failed_at(step, kind, NetworkKind::Telegram);
        for (step, kind, text) in [
            (
                "phone",
                Unauthorized,
                "Telegram has banned this phone number. Kourier cannot link it.",
            ),
            (
                "registration",
                Unsupported,
                "This phone number has no Telegram account. Create the account in the Telegram app first, then link it here.",
            ),
            (
                "update",
                Unsupported,
                "Telegram asks for a newer version of Kourier to log in.",
            ),
        ] {
            let v = tg(step, kind);
            assert_eq!(v.error, text, "{step}");
            assert!(!v.retry, "no Try Again at {step}");
            assert_eq!(v.leave, "OK", "{step}");
        }
        // A purchase before the code is sent: no Try Again, and no leave button, so the page keeps
        // the QR way (the view source test checks the switch).
        let purchase = tg("purchase", Unsupported);
        assert_eq!(
            purchase.error,
            "Telegram asks for a purchase before it sends a code to this number. Scan a QR code instead."
        );
        assert!(!purchase.retry && purchase.leave.is_empty());
        // The failure-only ids carry no network guard: the words are about the step.
        assert_eq!(
            failed_at("registration", Unsupported, NetworkKind::Signal).error,
            "This phone number has no Signal account. Create the account in the Signal app first, then link it here."
        );
        assert_eq!(
            failed_at("purchase", Unsupported, NetworkKind::Signal).error,
            "Signal asks for a purchase before it sends a code to this number. Scan a QR code instead."
        );
        // Kourier cannot take the code's way: Try Again stays (a new login may be sent another way).
        let code = tg("code", Unsupported);
        assert_eq!(
            code.error,
            "Telegram sent the code in a way Kourier cannot take. Scan a QR code instead."
        );
        assert!(code.retry && code.leave.is_empty());
        // A new attempt can take another address, or the QR way (no login e-mail is asked there):
        // Try Again stays, and so does the switch (fix round F6).
        let email = tg("email", Unsupported);
        assert_eq!(
            email.error,
            "Telegram does not accept this email address. Try another address, or scan a QR code instead."
        );
        assert!(email.retry && email.leave.is_empty());
        // The same pairs on WhatsApp keep today's words and its Try Again.
        let wa_banned = failed_at("phone", Unauthorized, NetworkKind::WhatsApp);
        assert_eq!(wa_banned.error, "The phone rejected the link.");
        assert!(wa_banned.retry && wa_banned.leave.is_empty());
        let wa_code = failed_at("code", Unsupported, NetworkKind::WhatsApp);
        assert_eq!(wa_code.error, "Something went wrong.");
        assert!(wa_code.retry);
        assert_eq!(
            failed_at("email", Unsupported, NetworkKind::WhatsApp).error,
            "Something went wrong."
        );
        // The wait is typed at every Telegram step that can be throttled. The code step is the
        // number's; the password and e-mail steps also follow a QR scan, where no number was
        // typed, so they speak of the account (post-rebase fix b).
        let wait = |step, network| {
            failed_at(
                step,
                RateLimited {
                    retry_after: Some(Duration::from_secs(300)),
                },
                network,
            )
            .error
        };
        for step in ["code", "password", "email", "email-code"] {
            let words = if step == "code" {
                "Telegram is limiting link attempts for this number. Wait 5 minutes before you try again."
            } else {
                "Telegram is limiting link attempts for this account. Wait 5 minutes before you try again."
            };
            assert_eq!(wait(step, NetworkKind::Telegram), words, "{step}");
            assert_eq!(
                wait(step, NetworkKind::WhatsApp),
                "Too many attempts. Wait a moment and try again.",
                "another network keeps the general words at {step}"
            );
        }
        // Without a wait the sentence still names the network.
        let no_wait = |step| {
            failed_at(
                step,
                RateLimited { retry_after: None },
                NetworkKind::Telegram,
            )
            .error
        };
        assert_eq!(
            no_wait("code"),
            "Telegram is limiting link attempts for this number. Wait a while before you try again."
        );
        for step in ["password", "email", "email-code"] {
            assert_eq!(
                no_wait(step),
                "Telegram is limiting link attempts for this account. Wait a while before you try again.",
                "{step}"
            );
        }
    }

    /// The account's wait (Telegram's password and e-mail steps, which a QR scan reaches with no
    /// number typed) names no number, in every length of wait, and with a translator that fills
    /// the plural's `%1` with the count as KI18n does (the network is `%2`).
    /// Mutant: the three steps back on `throttled_text` (the words speak of "this number").
    #[test]
    fn the_wait_after_a_qr_scan_names_no_number() {
        use std::time::Duration;
        struct LikeKi18n;
        impl crate::i18n::Translator for LikeKi18n {
            fn translate(&self, _context: &str, msgid: &str) -> String {
                msgid.to_owned()
            }
            fn translate_plural(&self, _c: &str, one: &str, many: &str, n: u64) -> String {
                let form = if n == 1 { one } else { many };
                form.replace("%1", &n.to_string())
            }
        }
        let said = |step: &str, secs: u64| {
            failed_at(
                step,
                ErrorKind::RateLimited {
                    retry_after: Some(Duration::from_secs(secs)),
                },
                NetworkKind::Telegram,
            )
            .error
        };
        crate::i18n::with_translator(LikeKi18n, || {
            for step in ["password", "email", "email-code"] {
                for (secs, wait) in [
                    (300, "5 minutes"),
                    (60, "1 minute"),
                    (61, "2 minutes"),
                    (7140, "119 minutes"),
                    (7200, "2 hours"),
                    (3 * 3600, "3 hours"),
                ] {
                    let text = said(step, secs);
                    assert_eq!(
                        text,
                        format!(
                            "Telegram is limiting link attempts for this account. Wait {wait} before you try again."
                        ),
                        "{step} {secs}"
                    );
                    assert!(!text.contains("number"), "{text}");
                }
            }
            // The code step is the number's and keeps its words.
            assert_eq!(
                said("code", 300),
                "Telegram is limiting link attempts for this number. Wait 5 minutes before you try again."
            );
        });
    }

    /// The second flow's button: Telegram's `phone` flow reads "Link with Phone Number Instead"
    /// (the message WhatsApp's `pair-code` has), `qr` keeps its words, another flow the descriptor's.
    /// Mutant: the `"phone"` arm removed (the descriptor's untranslated "Phone Number").
    #[test]
    fn the_phone_flow_switch_has_the_viewmodels_label() {
        assert_eq!(
            flow_switch_label("phone", "Phone Number"),
            "Link with Phone Number Instead"
        );
        assert_eq!(
            flow_switch_label("phone", "Phone Number"),
            flow_switch_label("pair-code", "Phone number")
        );
        assert_eq!(
            flow_switch_label("qr", "Scan a QR Code"),
            "Scan a QR Code Instead"
        );
        assert_eq!(
            flow_switch_label("passkey", "Use a\npasskey"),
            "Use a passkey"
        );
    }

    /// Telegram's turn-off question has its own words: no "30 days", the session stays, and the
    /// period is named as Telegram's own setting. Mutant: the Telegram arm removed.
    #[test]
    fn telegrams_turn_off_text_is_its_own() {
        let text = |entry: NetworkEntry| {
            network_choices(&[NetworkEntry::plain(NetworkKind::Signal), entry], false)[1]
                .turn_off_text
                .clone()
        };
        let tg = text(telegram(true));
        assert!(
            tg.starts_with("Kourier stops connecting to your Telegram account. Its session stays"),
            "{tg}"
        );
        assert!(!tg.contains("30"), "{tg}");
        assert!(tg.contains("time set in the Telegram app"), "{tg}");
        assert!(text(whatsapp(true)).contains("about 30 days"));
        assert!(text(threema(true)).contains("may remove a linked device"));
    }

    /// Settings says "Always sent for Telegram." only for a network that is ON at this start and
    /// always sends receipts; none gives an empty text (the page shows no line). Mutant: `on`
    /// ignored (an off Telegram is named).
    #[test]
    fn the_privacy_note_names_only_networks_that_are_on_and_always_send() {
        let note = |entries: &[NetworkEntry]| privacy_note(&network_choices(entries, false));
        let signal = NetworkEntry::plain(NetworkKind::Signal);
        assert_eq!(
            note(&[signal.clone(), telegram(true)]),
            "Always sent for Telegram."
        );
        assert_eq!(
            note(&[signal.clone(), telegram(false)]),
            "",
            "off: not named"
        );
        assert_eq!(
            note(&[signal.clone(), whatsapp(true)]),
            "",
            "WhatsApp does not always send"
        );
        assert_eq!(note(std::slice::from_ref(&signal)), "");
        // A network this build cannot log in to is not on, whatever it says of itself.
        let missing = NetworkEntry {
            missing: Some(NetworkMissing::ApiKey),
            ..telegram(true)
        };
        assert_eq!(note(&[signal.clone(), missing]), "");
        // Two such networks: one sentence each, joined by a space.
        let other = NetworkEntry {
            receipts_always_sent: true,
            ..threema(true)
        };
        assert_eq!(
            note(&[signal, telegram(true), other]),
            "Always sent for Telegram. Always sent for Threema."
        );
    }

    /// Only a network whose phone PUSHES its history at the link can "send a copy" of it, and
    /// only a list with a row that has a phone keeping the history says that "your phone stays
    /// the main device and keeps your message history" (KT-1b, finding 3 and the fix round's F3):
    /// Telegram serves its history from its servers on demand and has no main device, so a list
    /// whose every choosable row is Telegram (or an unavailable Signal and Telegram) says only
    /// that Kourier links as an additional device. Telegram beside Threema (a phone that keeps its
    /// history) is today's short sentence, beside WhatsApp the long one. Mutants: the rule counts
    /// every source but `None` again (the long sentence), the third branch dropped (the short
    /// sentence, which is false for Telegram).
    #[test]
    fn telegram_alone_never_promises_a_copy_or_a_main_phone() {
        const ONLY: &str = "Kourier links as an additional device.";
        const SHORT: &str = "Kourier links as an additional device: your phone stays the main device and keeps your message history.";
        const LONG: &str = "Kourier links as an additional device: your phone stays the main device, keeps your message history and can send a copy of it.";
        for on in [true, false] {
            // Signal is not in the build (a fixture run): its row cannot be chosen.
            let rows = network_choices(&[telegram(on)], true);
            assert!(!rows[0].available, "Signal's row is unavailable");
            assert!(rows[1].available && rows[1].history == HistorySource::OnDemand);
            assert_eq!(welcome_footnote(&rows), ONLY, "on: {on}");
        }
        // Threema beside it: a row whose phone keeps the history, today's short sentence.
        assert_eq!(
            welcome_footnote(&network_choices(&[telegram(true), threema(true)], true)),
            SHORT
        );
        // WhatsApp beside it brings the long sentence back.
        assert_eq!(
            welcome_footnote(&network_choices(&[telegram(true), whatsapp(true)], true)),
            LONG
        );
        // A Telegram this build cannot log in to is no choice: nothing choosable, today's sentence.
        let missing = NetworkEntry {
            missing: Some(NetworkMissing::ApiKey),
            ..telegram(true)
        };
        assert_eq!(welcome_footnote(&network_choices(&[missing], true)), SHORT);
        // The new sentence goes through the translator in the context the others have.
        struct Marks;
        impl crate::i18n::Translator for Marks {
            fn translate(&self, context: &str, msgid: &str) -> String {
                format!("{context}|{msgid}")
            }
            fn translate_plural(&self, _c: &str, one: &str, _m: &str, _n: u64) -> String {
                one.to_owned()
            }
        }
        let only = crate::i18n::with_translator(Marks, || {
            welcome_footnote(&network_choices(&[telegram(true)], true))
        });
        assert_eq!(only, format!("@info|{ONLY}"));
    }

    /// A network the build cannot log in to is listed for Settings with "Not available in this
    /// build", without a switch to flip on: no choice to start a login (not available, not on, no
    /// flows), no warning, its reason typed on the row. The other rows are untouched.
    #[test]
    fn a_network_the_build_cannot_log_in_to_is_a_settings_row_and_no_choice() {
        for missing in [NetworkMissing::ApiKey, NetworkMissing::Library] {
            for on in [true, false] {
                let entry = NetworkEntry {
                    missing: Some(missing),
                    ..telegram(on)
                };
                let rows =
                    network_choices(&[NetworkEntry::plain(NetworkKind::Signal), entry], false);
                assert_eq!(kinds(&rows), [NetworkKind::Signal, NetworkKind::Telegram]);
                let tg = &rows[1];
                assert_eq!(tg.description, "Not available in this build");
                assert_eq!(tg.off_description, "Not available in this build");
                assert_eq!(tg.missing, Some(missing));
                assert!(!tg.available && !tg.on && tg.flows.is_empty(), "{tg:?}");
                assert!(tg.experimental, "Settings lists the experimental networks");
                assert!(tg.risk_view().is_none(), "nothing to warn about");
            }
        }
        // The same entry without the reason is an ordinary experimental row.
        let rows = network_choices(&[telegram(false)], false);
        assert!(rows[1].missing.is_none() && rows[1].available && !rows[1].on);
        assert_eq!(rows[1].description, "Off · experimental");
    }
}
