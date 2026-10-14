//! The history transfer of a newly linked account as the UI shows it (architecture §14.2
//! item 14): what the link page and the account banner say while the phone prepares, while
//! the history downloads and while it is read; what is said once when it ends; the banner of
//! a failure that stays until the user acts; and the question before an account is removed
//! and linked again to transfer its history.
//!
//! The rules come from `UiEvent::HistoryTransfer` (`kourier_core::facade`): an event says
//! where the transfer stands now, the same state can come any number of times, an end comes
//! again at every start and connect until it is dismissed, and the bytes of a download can
//! step back. [`TransferBook`] is therefore a table of "the state now" per account plus two
//! facts the events do not carry: whether this UI sent the skip, and whether the end was
//! already shown.
//!
//! Nothing here holds text from a backend or from the history: only states, sizes and the
//! application's own sentences. The one text of a person is the account's own name and
//! number: in [`ReplaceQuestion::about`], and in front of a [`TransferView::status`] once the
//! banner tells several accounts apart; neither type prints it in `Debug`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kourier_model::{AccountId, AccountProgress, HistoryTransfer, HistoryTransferFailure};

use crate::i18n::{trc, trfc, trnfc};
use crate::time::file_size;

/// A second press of "Continue Without History" within this time of the first (a double
/// click) sends nothing more; a later one sends again, because Signal's backend may have
/// dropped the first skip without a word (the core had not taken the account yet).
pub const DOUBLE_PRESS: Duration = Duration::from_secs(2);

/// What the transfer view shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransferPhase {
    /// Waiting for the phone.
    Waiting,
    /// Downloading the history.
    Downloading,
    /// Reading what was downloaded.
    Reading,
    /// The transfer failed; the banner stays until the user acts.
    Failed,
    /// No transfer runs (any more), but the account still fetches the profiles of its
    /// contacts and the groups the server has not given yet (the tail of every fresh link).
    Updating,
}

impl TransferPhase {
    /// Stable name for QML (`waiting`, `downloading`, `reading`, `failed`, `updating`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Downloading => "downloading",
            Self::Reading => "reading",
            Self::Failed => "failed",
            Self::Updating => "updating",
        }
    }
}

/// One history transfer as it is shown in the account banner: one line, bytes, bar, button.
/// (The link page shows none: the chats are usable as soon as the account is linked.)
///
/// Its `status` can carry the name of an account once the banner tells several accounts
/// apart, so `Debug` prints the account's id and the phase only.
#[derive(Clone, PartialEq)]
pub struct TransferView {
    /// The account.
    pub account: AccountId,
    /// What it shows.
    pub phase: TransferPhase,
    /// The one line of the banner; for a failure, what happened and what can be done.
    pub status: String,
    /// The bytes of a download ("12.3 MB of 45.6 MB", or "12.3 MB" while the size is
    /// unknown); empty otherwise.
    pub detail: String,
    /// How far a download is, 0 … 1; `-1` when that is not known (waiting, reading, a
    /// download whose size the server has not stated).
    pub progress: f64,
    /// Label of the button that ends the wait or the download ("Continue Without History");
    /// empty when the state cannot be skipped.
    pub skip: String,
    /// Label of a failure's "Try Again…" (it opens the question, [`ReplaceQuestion`]); empty
    /// when trying again cannot help.
    pub retry: String,
    /// Label of a failure's "Dismiss"; empty while the transfer runs.
    pub dismiss: String,
}

crate::safe_debug!(TransferView { account, phase });

/// What is said once when a transfer ended without a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndNotice {
    /// The history was transferred.
    Transferred,
    /// No history was transferred, and not because this UI skipped it.
    NotTransferred,
}

impl EndNotice {
    /// The sentence.
    pub fn text(self) -> String {
        match self {
            Self::Transferred => trc(
                "@info:status a passing notice",
                "The message history was transferred.",
            ),
            Self::NotTransferred => trc(
                "@info:status a passing notice",
                "The message history was not transferred.",
            ),
        }
        .into_owned()
    }
}

/// What the caller does after [`TransferBook::on_state`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// What [`TransferBook::view`] answers for the account changed.
    pub changed: bool,
    /// A notice to show once.
    pub notice: Option<EndNotice>,
    /// Send `DismissHistoryTransfer` for the account: its end was shown (now, or before this
    /// report of it, in which case the earlier dismissal did not reach the backend in time).
    pub dismiss: bool,
}

/// What is known of one account's transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    /// The state as last reported.
    state: HistoryTransfer,
    /// This UI sent the skip of the transfer that runs or ran.
    skipped_here: bool,
    /// When the skip was sent, while the backend has reported nothing since: a second press
    /// within [`DOUBLE_PRESS`] of it (a double click on "Continue Without History") sends
    /// nothing more. The next report clears it, and so does the time: Signal's backend
    /// may have dropped the skip (the core had not taken the account yet, so it reports
    /// nothing), and a button that stayed dead until the phone's next report would be a
    /// stuck control for minutes.
    skip_sent: Option<Instant>,
    /// The end in `state` was shown and dismissed: its notice went out (done, skipped), or
    /// the user closed its banner (failed). A repeated report of it shows nothing again.
    settled: bool,
}

/// The history transfers of all accounts, by what was last reported.
#[derive(Clone, Debug, Default)]
pub struct TransferBook {
    entries: HashMap<AccountId, Entry>,
    /// The latest `UiEvent::AccountProgress` of each account whose work is not finished.
    progress: HashMap<AccountId, AccountProgress>,
    /// The most profiles and groups left seen in one account's reports since its work began:
    /// the whole the "Updating" bar measures against (the reports say only what is left).
    peak: HashMap<AccountId, u64>,
}

impl TransferBook {
    /// `UiEvent::HistoryTransfer`: the transfer of `account` stands at `state` now.
    pub fn on_state(&mut self, account: AccountId, state: HistoryTransfer) -> Outcome {
        let prev = self.entries.get(&account).copied();
        let shown_before = prev.is_some_and(|e| e.shows());
        // The same end as the one already shown: only the dismissal is owed again.
        if let Some(e) = prev
            && e.settled
            && e.state == state
        {
            return Outcome {
                changed: false,
                notice: None,
                dismiss: true,
            };
        }
        let mut entry = Entry {
            state,
            // A state that is not over after an end (or after nothing) is a new transfer:
            // nobody has skipped it yet.
            skipped_here: prev.is_some_and(|e| e.skipped_here && !e.state.is_over()),
            skip_sent: None,
            settled: false,
        };
        let mut out = Outcome::default();
        match state {
            HistoryTransfer::Done => {
                out.notice = Some(EndNotice::Transferred);
                out.dismiss = true;
                entry.settled = true;
            }
            HistoryTransfer::Skipped => {
                // Our own skip needs no word: the user just pressed the button.
                if !entry.skipped_here {
                    out.notice = Some(EndNotice::NotTransferred);
                }
                out.dismiss = true;
                entry.settled = true;
            }
            // A failure stays until the user acts; a state that is not over is shown as
            // it is.
            HistoryTransfer::Failed(_)
            | HistoryTransfer::Waiting
            | HistoryTransfer::Downloading { .. }
            | HistoryTransfer::Reading => {}
        }
        out.changed = shown_before || entry.shows();
        if prev.is_some_and(|e| e.state == state && e.settled == entry.settled) {
            // The very same state again: nothing to redraw.
            out.changed = false;
        }
        self.entries.insert(account, entry);
        out
    }

    /// The user pressed "Continue Without History": `true` when the transfer of `account`
    /// can be skipped now (the caller sends `SkipHistoryTransfer`). The state stays what it
    /// is until the backend reports the end. A press within [`DOUBLE_PRESS`] of the last
    /// skip that no report has answered is `false`: one skip per press of the button, not
    /// per click of a double click (`buttons.md` §3, scratch `doubleskip` sent two). Any
    /// later press sends again.
    pub fn skip(&mut self, account: AccountId) -> bool {
        self.skip_at(account, Instant::now())
    }

    /// [`Self::skip`] at a given time (tests bring their own clock).
    pub fn skip_at(&mut self, account: AccountId, now: Instant) -> bool {
        match self.entries.get_mut(&account) {
            Some(e)
                if e.state.can_skip()
                    && !e
                        .skip_sent
                        .is_some_and(|at| now.saturating_duration_since(at) < DOUBLE_PRESS) =>
            {
                e.skipped_here = true;
                e.skip_sent = Some(now);
                true
            }
            _ => false,
        }
    }

    /// The user closed the banner of a failed transfer: `true` when there was one to close
    /// (the caller sends `DismissHistoryTransfer`). Should the backend report the same
    /// failure once more, [`Self::on_state`] asks for the dismissal again and shows nothing.
    pub fn dismiss(&mut self, account: AccountId) -> bool {
        match self.entries.get_mut(&account) {
            Some(e) if matches!(e.state, HistoryTransfer::Failed(_)) && !e.settled => {
                e.settled = true;
                true
            }
            _ => false,
        }
    }

    /// The account was logged out (unlinked by the phone, or by the server). Its backend
    /// deleted a transfer that ran with the login and reports nothing about it, so a state
    /// that is not over is forgotten here too, with the skip this UI may have sent: `true`
    /// when something that was shown is gone. An end stays: a failure nobody dismissed is
    /// still what happened, and an end that was said is not said again.
    pub fn logged_out(&mut self, account: AccountId) -> bool {
        if self.running(account) {
            self.entries.remove(&account);
            true
        } else {
            false
        }
    }

    /// The account is gone: nothing of its transfer is shown any more.
    pub fn remove(&mut self, account: AccountId) -> bool {
        let progress = self.forget_progress(account);
        self.entries.remove(&account).is_some_and(|e| e.shows()) || progress
    }

    /// `UiEvent::AccountProgress`: the numbers of `account` now; `true` when what is shown
    /// changed. A finished report forgets the account's numbers.
    pub fn on_progress(&mut self, account: AccountId, progress: AccountProgress) -> bool {
        if progress.finished {
            return self.forget_progress(account);
        }
        let left = updates_left(progress);
        let peak = self.peak.entry(account).or_default();
        *peak = (*peak).max(left);
        self.progress.insert(account, progress) != Some(progress)
    }

    /// The numbers of `account` while its work is not finished.
    pub fn progress(&self, account: AccountId) -> Option<AccountProgress> {
        self.progress.get(&account).copied()
    }

    /// The account was logged out or removed: its backend's work stopped with it, and no
    /// finished report follows.
    pub fn forget_progress(&mut self, account: AccountId) -> bool {
        self.peak.remove(&account);
        self.progress.remove(&account).is_some()
    }

    /// Every account's numbers are forgotten: a channel that lagged may have dropped the one
    /// `finished` report, which is never sent again, and an update that nobody ends would show
    /// (and ask before quitting) for ever. A changed report brings the numbers back. `true`
    /// when something that was shown is gone.
    pub fn forget_all_progress(&mut self) -> bool {
        self.peak.clear();
        let shown = !self.progress.is_empty();
        self.progress.clear();
        shown
    }

    /// What to show for `account` once no transfer of it shows: the profiles and groups it
    /// still fetches ("Updating contacts and groups: 120 profiles, 8 groups left", a bar
    /// against the most seen left); `None` when its work is finished or it reports none.
    pub fn updating(&self, account: AccountId) -> Option<TransferView> {
        let p = self.progress(account)?;
        let left = updates_left(p);
        let mut v = view(account, HistoryTransfer::Reading);
        v.phase = TransferPhase::Updating;
        v.status = updating_text(p);
        v.progress = match self.peak.get(&account).copied() {
            Some(peak) if peak > 0 => fraction(peak - left.min(peak), Some(peak)),
            _ => -1.0,
        };
        Some(v)
    }

    /// Whether a transfer of `account` runs (waiting, downloading, reading).
    pub fn running(&self, account: AccountId) -> bool {
        self.entries
            .get(&account)
            .is_some_and(|e| !e.state.is_over())
    }

    /// What to show for `account`: its running transfer, or its failed one until it is
    /// dismissed; `None` otherwise.
    pub fn view(&self, account: AccountId) -> Option<TransferView> {
        let e = self.entries.get(&account).filter(|e| e.shows())?;
        let mut v = view(account, e.state);
        if e.state == HistoryTransfer::Reading
            && let Some(p) = self.progress(account).filter(AccountProgress::importing)
        {
            counted_import(&mut v, p);
        }
        Some(v)
    }
}

impl Entry {
    fn shows(&self) -> bool {
        match self.state {
            HistoryTransfer::Waiting
            | HistoryTransfer::Downloading { .. }
            | HistoryTransfer::Reading => true,
            HistoryTransfer::Failed(_) => !self.settled,
            HistoryTransfer::Done | HistoryTransfer::Skipped => false,
        }
    }
}

/// How far a download is: `done / total` within 0 … 1, or `-1` while the size is unknown
/// (not stated, or stated as nothing). More bytes than the stated size is a full bar; fewer
/// bytes than a moment ago is simply a shorter bar (a restart counts from the file on disk).
pub fn fraction(done: u64, total: Option<u64>) -> f64 {
    match total {
        Some(total) if total > 0 => {
            // (Sizes are far below 2^53: the division is exact enough for a bar.)
            #[allow(clippy::cast_precision_loss)]
            let f = done.min(total) as f64 / total as f64;
            f.clamp(0.0, 1.0)
        }
        _ => -1.0,
    }
}

/// The bytes of a download: "12.3 MB of 45.6 MB", or "12.3 MB" while the size is unknown.
pub fn bytes_text(done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => trfc(
            "@info:progress how much of a download is here, %1 the part, %2 the whole, both sizes with their units",
            "%1 of %2",
            &[&file_size(done.min(total)), &file_size(total)],
        ),
        _ => file_size(done),
    }
}

fn skip_label() -> String {
    trc(
        "@action:button stop waiting for the message history",
        "Continue Without History",
    )
    .into_owned()
}

/// The view of `state` (one that is shown: not done, not skipped).
fn view(account: AccountId, state: HistoryTransfer) -> TransferView {
    let mut v = TransferView {
        account,
        phase: TransferPhase::Waiting,
        status: String::new(),
        detail: String::new(),
        progress: -1.0,
        skip: String::new(),
        retry: String::new(),
        dismiss: String::new(),
    };
    match state {
        HistoryTransfer::Waiting | HistoryTransfer::Done | HistoryTransfer::Skipped => {
            v.phase = TransferPhase::Waiting;
            // (A transfer exists only when the phone's user already agreed to it: on Signal
            // the phone asks at the scan, before it links. While this waits the phone
            // prepares and uploads the history.)
            v.status = trc(
                "@info:status banner of an account",
                "Waiting for your phone to prepare the message history…",
            )
            .into_owned();
            v.skip = skip_label();
        }
        HistoryTransfer::Downloading { done, total } => {
            v.phase = TransferPhase::Downloading;
            v.status = trc(
                "@info:status banner of an account",
                "Downloading message history…",
            )
            .into_owned();
            v.detail = bytes_text(done, total);
            v.progress = fraction(done, total);
            v.skip = skip_label();
        }
        HistoryTransfer::Reading => {
            v.phase = TransferPhase::Reading;
            // (No "keep Kourier open": a quit goes on at the next start, and the question
            // before quitting says so.)
            v.status = trc(
                "@info:status banner of an account",
                "Importing messages… New messages arrive afterwards.",
            )
            .into_owned();
        }
        HistoryTransfer::Failed(kind) => {
            v.phase = TransferPhase::Failed;
            // (A failure is one message in the banner.)
            v.status = failure_text(kind);
            if kind != HistoryTransferFailure::UpdateApp {
                v.retry = trc(
                    "@action:button transfer the message history again; it opens a question",
                    "Try Again…",
                )
                .into_owned();
            }
            v.dismiss = trc("@action:button", "Dismiss").into_owned();
        }
    }
    v
}

/// The messages of an import as they are delivered: "Importing history: 12,400 of 50,000
/// messages. New messages arrive afterwards." with a bar that knows how far it is. The reason
/// stays in the line: the chats are usable meanwhile and a message sent now goes out at once
/// (link feedback L6), but nothing new arrives until the import is delivered.
fn counted_import(v: &mut TransferView, p: AccountProgress) {
    let done = p.messages_done.min(p.messages_total);
    v.status = trnfc(
        "@info:status banner of an account while an imported message history fills the chats, %1 all the messages, %2 the ones there so far",
        "Importing history: %2 of %1 message. New messages arrive afterwards.",
        "Importing history: %2 of %1 messages. New messages arrive afterwards.",
        p.messages_total,
        &[&group_digits(p.messages_total), &group_digits(done)],
    );
    v.progress = fraction(done, Some(p.messages_total));
}

/// The profiles and groups still to come in one report.
fn updates_left(p: AccountProgress) -> u64 {
    u64::from(p.profiles_left) + u64::from(p.groups_left)
}

/// "Updating contacts and groups: 120 profiles, 8 groups left", with only the counts that
/// are not zero; without either (a sync of the link still pending) the bare line.
fn updating_text(p: AccountProgress) -> String {
    let profiles = (p.profiles_left > 0).then(|| {
        trnfc(
            "@info:status a count in the line of an account that fetches its contacts and groups, %1 the number",
            "%1 profile",
            "%1 profiles",
            u64::from(p.profiles_left),
            &[&group_digits(u64::from(p.profiles_left))],
        )
    });
    let groups = (p.groups_left > 0).then(|| {
        trnfc(
            "@info:status a count in the line of an account that fetches its contacts and groups, %1 the number",
            "%1 group",
            "%1 groups",
            u64::from(p.groups_left),
            &[&group_digits(u64::from(p.groups_left))],
        )
    });
    match (profiles, groups) {
        (Some(a), Some(b)) => trfc(
            "@info:status banner of an account that fetches its contacts and groups, %1 the profiles left as in 120 profiles, %2 the groups left as in 8 groups",
            "Updating contacts and groups: %1, %2 left",
            &[&a, &b],
        ),
        (Some(a), None) | (None, Some(a)) => trfc(
            "@info:status banner of an account that fetches its contacts and groups, %1 what is left, as in 120 profiles or 8 groups",
            "Updating contacts and groups: %1 left",
            &[&a],
        ),
        (None, None) => trc(
            "@info:status banner of an account that fetches its contacts and groups",
            "Updating contacts and groups…",
        )
        .into_owned(),
    }
}

/// A count with its thousands grouped ("12,400"; the separator is the translator's).
pub fn group_digits(n: u64) -> String {
    // A front-end that fills a plural's count in itself writes every count: the same digit
    // groups for both numbers of "12,400 of 50,000".
    if let Some(written) = crate::i18n::number(n) {
        return written;
    }
    let digits = n.to_string();
    if digits.len() <= 4 {
        // "1200" reads as well as "1,200", and a year-like number stays whole.
        return digits;
    }
    let sep = trc(
        "@info the character between the groups of three digits of a large number, as in 12,400; many languages use a no-break space",
        ",",
    );
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * sep.len());
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push_str(&sep);
        }
        out.push(c);
    }
    out
}

/// What happened and what the user can do, one text per kind of failure.
pub fn failure_text(kind: HistoryTransferFailure) -> String {
    match kind {
        HistoryTransferFailure::LinkAgain => trc(
            "@info:status banner, the transfer of the message history failed",
            "Your phone could not prepare the message history. To transfer it, link this computer again.",
        ),
        HistoryTransferFailure::UpdateApp => trc(
            "@info:status banner, the transfer of the message history failed",
            "This version of Kourier cannot read the message history your phone sent. Update Kourier first; you can then transfer the history from Settings.",
        ),
        HistoryTransferFailure::Corrupt => trc(
            "@info:status banner, the transfer of the message history failed",
            "The message history arrived damaged and was not imported. You can transfer it again.",
        ),
        HistoryTransferFailure::TooLarge => trc(
            "@info:status banner, the transfer of the message history failed",
            "The message history is larger than Kourier accepts and was not transferred. Trying again helps only once it is smaller on your phone.",
        ),
        HistoryTransferFailure::Download => trc(
            "@info:status banner, the transfer of the message history failed",
            "The message history could not be downloaded. Check your connection, then try again.",
        ),
        HistoryTransferFailure::Internal => trc(
            "@info:status banner, the transfer of the message history failed",
            "The message history could not be stored on this computer. Check the free disk space, then try again.",
        ),
        HistoryTransferFailure::Incomplete => trc(
            "@info:status banner, the transfer of the message history failed",
            "Only part of the message history was transferred. The messages that arrived stay; you can transfer the history again.",
        ),
    }
    .into_owned()
}

/// The question before an account is removed from this computer and linked again so that
/// the phone can transfer its history ("Transfer History from Phone…", and "Try Again…" of
/// a failed transfer). The default answer is to cancel.
#[derive(Clone, PartialEq, Eq)]
pub struct ReplaceQuestion {
    /// The account that would be removed.
    pub account: AccountId,
    /// The dialog's title.
    pub title: String,
    /// Which account it is about: its name and, where known, its number (personal data:
    /// shown as plain text, never logged).
    pub about: String,
    /// What happens, in plain words, one paragraph each.
    pub paragraphs: Vec<String>,
    /// The confirming button, named by what it does.
    pub confirm: String,
    /// The cancelling button (the default).
    pub cancel: String,
}

crate::safe_debug!(ReplaceQuestion { account });

/// Which account, in one line: its name and, after a dot, its number or address; where the
/// network has none, the name of its messenger; the name alone when that says nothing more.
/// The question and the settings row of "Transfer History from Phone…" both show it (text
/// of the account: plain text in the UI, never logged).
pub fn account_line(name: &str, number: &str, network_name: &str) -> String {
    let detail = if number.is_empty() {
        network_name
    } else {
        number
    };
    if detail.is_empty() || detail == name {
        name.to_owned()
    } else {
        trfc(
            "@info an account, %1 its name, %2 its phone number or address, or the name of its messenger",
            "%1 · %2",
            &[name, detail],
        )
    }
}

/// Which link of its identity an account is, for two accounts that look the same: the device
/// number the network gave this computer, and the day it was linked (what the phone lists
/// beside each linked device). Empty when neither is known.
pub fn device_label(device: Option<u32>, linked: &str) -> String {
    match (device, linked.is_empty()) {
        (Some(n), false) => trfc(
            "@info which of two links of one account this is, %1 the device number the network gave this computer, %2 the date it was linked",
            "device %1 · linked %2",
            &[&n.to_string(), linked],
        ),
        (Some(n), true) => trfc(
            "@info which of two links of one account this is, %1 the device number the network gave this computer",
            "device %1",
            &[&n.to_string()],
        ),
        (None, false) => trfc(
            "@info which of two links of one account this is, %1 the date it was linked",
            "linked %1",
            &[linked],
        ),
        (None, true) => String::new(),
    }
}

/// An account's line ([`account_line`]) with its device ([`device_label`]); the line alone
/// when `device` is empty.
pub fn with_device(about: &str, device: &str) -> String {
    if device.is_empty() {
        about.to_owned()
    } else {
        trfc(
            "@info an account told apart from another that looks the same, %1 its name and number, %2 which device it is",
            "%1 · %2",
            &[about, device],
        )
    }
}

/// A passing notice about one of several accounts: the account's name in front of it.
pub fn named_notice(account_name: &str, notice: &str) -> String {
    trfc(
        "@info:status a passing notice about one of several accounts, %1 the name of the account, %2 what happened",
        "%1: %2",
        &[account_name, notice],
    )
}

/// Label of the account action that opens the question.
pub fn transfer_action_label() -> String {
    trc(
        "@action:button remove an account and link it again to get its message history",
        "Transfer History from Phone…",
    )
    .into_owned()
}

/// Builds the question for `account`, described by `about`.
pub fn replace_question(account: AccountId, about: String) -> ReplaceQuestion {
    ReplaceQuestion {
        account,
        title: trc(
            "@title:dialog question before an account is removed and linked again",
            "Remove and Link Again?",
        )
        .into_owned(),
        about,
        paragraphs: vec![
            trc(
                "@info question before an account is removed and linked again",
                "To transfer the message history, Kourier removes this account's chats, messages and files from this computer and links the account again. Your phone then asks whether to transfer the message history.",
            )
            .into_owned(),
            trc(
                "@info question before an account is removed and linked again",
                "Whatever exists only on this computer is lost, and older attachments may not be transferred.",
            )
            .into_owned(),
            trc(
                "@info question before an account is removed and linked again",
                "You need your phone to link again. If you cancel linking afterwards, the account stays removed from this computer.",
            )
            .into_owned(),
        ],
        confirm: trc(
            "@action:button remove an account from this computer and link it again",
            "Remove and Link Again",
        )
        .into_owned(),
        cancel: trc("@action:button", "Cancel").into_owned(),
    }
}

/// The question before an action that would cancel a link in progress (`buttons.md` F3:
/// opening Settings, "Link Again" of another account, or "Remove and Link Again" while the
/// link page waits for its QR code to be scanned). The default answer keeps the link.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AbandonQuestion {
    /// The dialog's title.
    pub title: String,
    /// What happens, in plain words.
    pub text: String,
    /// The button that cancels the link and goes on.
    pub confirm: String,
    /// The button that keeps the link (the default).
    pub cancel: String,
}

/// The question before a link to `network` (the messenger's name) in progress is cancelled.
pub fn abandon_question(network: &str) -> AbandonQuestion {
    AbandonQuestion {
        title: trc(
            "@title:dialog question before a link in progress is cancelled",
            "Stop Linking?",
        )
        .into_owned(),
        text: abandon_paragraph(network),
        confirm: trc("@action:button cancel a link in progress", "Stop Linking").into_owned(),
        cancel: trc("@action:button keep a link in progress", "Continue Linking").into_owned(),
    }
}

/// What the action the user pressed does to a link in progress, one paragraph. It names what
/// cancels the link, not "going on": the dialog's default button says "Continue Linking",
/// and a text that says that continuing cancels it contradicts the button (user review of
/// L1).
fn abandon_paragraph(network: &str) -> String {
    trfc(
        "@info question before a link in progress is cancelled, %1 the name of a messenger",
        "Linking a %1 account has not finished. Opening this now cancels the link. You can link the account again later.",
        &[network],
    )
}

/// The same for the question of "Remove and Link Again": there the account is removed, which
/// cancels the other link.
fn replace_abandon_paragraph(network: &str) -> String {
    trfc(
        "@info question before an account is replaced while another link is in progress, %1 the name of a messenger",
        "Linking another %1 account has not finished. Removing this account now cancels that link. You can link that account again later.",
        &[network],
    )
}

/// What still runs when the window is closed (`buttons.md` F8): the question before the
/// application quits names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QuitWork {
    /// A history transfer: waiting for the phone, downloading or importing.
    Import,
    /// The contacts and groups a fresh link still fetches.
    Update,
}

/// The question before the application quits while `work` runs. Nothing is lost (the work
/// goes on at the next start); the question is there because the window said "Keep Kourier
/// open". The default answer keeps the application open.
pub fn quit_question(work: QuitWork) -> AbandonQuestion {
    let (title, text) = match work {
        QuitWork::Import => (
            trc(
                "@title:dialog question before the application quits while a message history is transferred",
                "Quit While Importing History?",
            ),
            trc(
                "@info question before the application quits while a message history is transferred",
                "Kourier is still importing your message history. It goes on at the next start.",
            ),
        ),
        QuitWork::Update => (
            trc(
                "@title:dialog question before the application quits while a new link fetches its contacts and groups",
                "Quit While Updating Contacts and Groups?",
            ),
            trc(
                "@info question before the application quits while a new link fetches its contacts and groups",
                "Kourier is still fetching your contacts and groups. It goes on at the next start.",
            ),
        ),
    };
    AbandonQuestion {
        title: title.into_owned(),
        text: text.into_owned(),
        confirm: trc("@action:button quit the application", "Quit").into_owned(),
        cancel: trc("@action:button", "Cancel").into_owned(),
    }
}

/// [`replace_question`] when it would also cancel a link in progress to `network`.
pub fn replace_question_cancelling(
    account: AccountId,
    about: String,
    network: &str,
) -> ReplaceQuestion {
    let mut q = replace_question(account, about);
    q.paragraphs.push(replace_abandon_paragraph(network));
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: AccountId = AccountId(1);
    const B: AccountId = AccountId(2);

    fn downloading(done: u64, total: Option<u64>) -> HistoryTransfer {
        HistoryTransfer::Downloading { done, total }
    }

    #[test]
    fn a_state_that_is_repeated_changes_nothing_and_asks_for_nothing() {
        let mut book = TransferBook::default();
        let first = book.on_state(A, HistoryTransfer::Waiting);
        assert_eq!(
            first,
            Outcome {
                changed: true,
                notice: None,
                dismiss: false
            }
        );
        for _ in 0..3 {
            assert_eq!(
                book.on_state(A, HistoryTransfer::Waiting),
                Outcome::default()
            );
        }
        let v = book.view(A).expect("shown");
        assert_eq!(v.phase, TransferPhase::Waiting);
        assert_eq!(
            v.status,
            "Waiting for your phone to prepare the message history…"
        );
        assert_eq!(v.skip, "Continue Without History");
        assert_eq!(v.progress, -1.0);
        assert!(v.detail.is_empty() && v.retry.is_empty() && v.dismiss.is_empty());
        assert!(book.running(A));
    }

    fn counts(done: u64, total: u64) -> AccountProgress {
        AccountProgress {
            messages_done: done,
            messages_total: total,
            ..AccountProgress::default()
        }
    }

    #[test]
    fn the_read_counts_the_messages_as_they_arrive_and_forgets_them_when_finished() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Reading);
        // Before the first count the read has no measure.
        let v = book.view(A).expect("shown");
        assert_eq!(
            v.status,
            "Importing messages… New messages arrive afterwards."
        );
        assert_eq!(v.progress, -1.0);
        assert!(book.on_progress(A, counts(12_400, 50_000)));
        assert!(
            !book.on_progress(A, counts(12_400, 50_000)),
            "the same numbers"
        );
        let v = book.view(A).expect("shown");
        assert_eq!(
            v.status,
            "Importing history: 12,400 of 50,000 messages. New messages arrive afterwards."
        );
        assert!((v.progress - 0.248).abs() < 1e-9);
        // Another account's numbers are its own; one message is one.
        book.on_state(B, HistoryTransfer::Reading);
        book.on_progress(B, counts(1, 1));
        assert_eq!(
            book.view(B).expect("shown").status,
            "Importing history: 1 of 1 message. New messages arrive afterwards."
        );
        // More than in all is all.
        book.on_progress(A, counts(60_000, 50_000));
        assert_eq!(
            book.view(A).expect("shown").status,
            "Importing history: 50,000 of 50,000 messages. New messages arrive afterwards."
        );
        // The profiles and groups that follow count no messages: the read has no measure.
        book.on_progress(
            A,
            AccountProgress {
                profiles_left: 3,
                ..AccountProgress::default()
            },
        );
        assert_eq!(book.view(A).expect("shown").progress, -1.0);
        assert_eq!(book.progress(A).map(|p| p.profiles_left), Some(3));
        // Finished: nothing is left of the numbers; a second finish changes nothing.
        assert!(book.on_progress(A, AccountProgress::finished()));
        assert!(!book.on_progress(A, AccountProgress::finished()));
        assert_eq!(book.progress(A), None);
        // A logout or a removal takes the numbers too.
        assert!(book.forget_progress(B));
        book.on_progress(B, counts(1, 2));
        assert!(book.remove(B));
        assert_eq!(book.progress(B), None);
    }

    fn left(profiles: u32, groups: u32) -> AccountProgress {
        AccountProgress {
            profiles_left: profiles,
            groups_left: groups,
            ..AccountProgress::default()
        }
    }

    #[test]
    fn the_update_after_a_link_counts_what_is_left_against_the_most_seen() {
        let mut book = TransferBook::default();
        assert!(book.updating(A).is_none(), "nothing reported");
        book.on_progress(A, left(120, 8));
        let v = book.updating(A).expect("shown");
        assert_eq!(v.phase, TransferPhase::Updating);
        assert_eq!(
            v.status,
            "Updating contacts and groups: 120 profiles, 8 groups left"
        );
        assert_eq!(v.progress, 0.0, "nothing done yet");
        assert!(v.skip.is_empty() && v.retry.is_empty() && v.dismiss.is_empty());
        // Half of the most seen is done.
        book.on_progress(A, left(60, 4));
        assert!((book.updating(A).expect("shown").progress - 0.5).abs() < 1e-9);
        // More work queued than ever: the whole grows, the bar never goes past full.
        book.on_progress(A, left(200, 0));
        let v = book.updating(A).expect("shown");
        assert_eq!(v.status, "Updating contacts and groups: 200 profiles left");
        assert_eq!(v.progress, 0.0);
        book.on_progress(A, left(0, 1));
        let v = book.updating(A).expect("shown");
        assert_eq!(v.status, "Updating contacts and groups: 1 group left");
        assert!((v.progress - 0.995).abs() < 1e-9);
        book.on_progress(A, left(1, 0));
        assert_eq!(
            book.updating(A).expect("shown").status,
            "Updating contacts and groups: 1 profile left"
        );
        // Nothing counted, but not finished (a sync of the link is still pending).
        book.on_progress(A, left(0, 0));
        let v = book.updating(A).expect("shown");
        assert_eq!(v.status, "Updating contacts and groups…");
        assert_eq!(v.progress, 1.0);
        // Finished: gone, and the next work measures from its own start.
        book.on_progress(A, AccountProgress::finished());
        assert!(book.updating(A).is_none());
        book.on_progress(A, left(10, 0));
        assert_eq!(book.updating(A).expect("shown").progress, 0.0);
        // Thousands are grouped here too.
        book.on_progress(B, left(12_400, 2));
        assert_eq!(
            book.updating(B).expect("shown").status,
            "Updating contacts and groups: 12,400 profiles, 2 groups left"
        );
    }

    #[test]
    fn large_counts_group_their_thousands() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_200, "1200"),
            (12_400, "12,400"),
            (123_456, "123,456"),
            (1_234_567, "1,234,567"),
        ];
        for (n, text) in cases {
            assert_eq!(group_digits(n), text, "{n}");
        }
    }

    /// A front-end whose plural fills its count in itself (KI18n writes `%L1`, by the
    /// locale's numbers) writes the other count of the line the same way.
    #[test]
    fn both_counts_of_the_import_are_written_alike_when_the_front_end_writes_numbers() {
        struct Spaced;
        impl crate::i18n::Translator for Spaced {
            fn translate(&self, _context: &str, msgid: &str) -> String {
                msgid.to_owned()
            }
            fn translate_plural(&self, _c: &str, one: &str, many: &str, n: u64) -> String {
                let form = if n == 1 { one } else { many };
                form.replace("%1", &self.number(n).expect("written"))
            }
            fn number(&self, n: u64) -> Option<String> {
                Some(match n {
                    50_000 => "50\u{a0}000".to_owned(),
                    12_400 => "12\u{a0}400".to_owned(),
                    other => other.to_string(),
                })
            }
        }
        crate::i18n::with_translator(Spaced, || {
            let mut book = TransferBook::default();
            book.on_state(A, HistoryTransfer::Reading);
            book.on_progress(A, counts(12_400, 50_000));
            assert_eq!(
                book.view(A).expect("shown").status,
                "Importing history: 12\u{a0}400 of 50\u{a0}000 messages. New messages arrive afterwards."
            );
        });
    }

    #[test]
    fn a_download_says_its_bytes_and_copes_with_a_count_that_steps_back() {
        let mut book = TransferBook::default();
        let mb = 1_000_000;
        book.on_state(A, downloading(30 * mb, Some(40 * mb)));
        let v = book.view(A).expect("shown");
        assert_eq!(v.phase, TransferPhase::Downloading);
        assert_eq!(v.status, "Downloading message history…");
        assert_eq!(v.detail, "30.0 MB of 40.0 MB");
        assert_eq!(v.progress, 0.75);
        assert_eq!(v.skip, "Continue Without History");
        // After a restart the count starts from the file on disk: fewer bytes than before.
        let back = book.on_state(A, downloading(10 * mb, Some(40 * mb)));
        assert!(back.changed && back.notice.is_none() && !back.dismiss);
        let v = book.view(A).expect("shown");
        assert_eq!(
            (v.detail.as_str(), v.progress),
            ("10.0 MB of 40.0 MB", 0.25)
        );
        // The size is not stated yet: only the bytes, and a bar that does not guess.
        book.on_state(A, downloading(5 * mb, None));
        let v = book.view(A).expect("shown");
        assert_eq!((v.detail.as_str(), v.progress), ("5.0 MB", -1.0));
        // A stated size of nothing is no size, and more bytes than the size is a full bar.
        assert_eq!(fraction(7, Some(0)), -1.0);
        assert_eq!(bytes_text(7, Some(0)), "7 B");
        assert_eq!(fraction(50, Some(40)), 1.0);
        assert_eq!(bytes_text(50 * mb, Some(40 * mb)), "40.0 MB of 40.0 MB");
        assert_eq!(fraction(0, Some(40)), 0.0);
    }

    #[test]
    fn reading_cannot_be_skipped() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Reading);
        let v = book.view(A).expect("shown");
        assert_eq!(v.phase, TransferPhase::Reading);
        assert!(
            !v.status.contains("Keep Kourier open"),
            "a read goes on at the next start: no request to keep the application open"
        );
        assert!(v.status.contains("New messages arrive afterwards"));
        assert!(v.skip.is_empty(), "no button to skip a read");
        assert_eq!(v.progress, -1.0);
        assert!(!book.skip(A), "and the skip is refused");
    }

    /// L5 fix 1: a double click sends one skip, but a skip Signal's backend dropped without
    /// a report (the core had not taken the account yet) must never leave a dead button: a
    /// press later than `DOUBLE_PRESS` sends again, and so does a press after any report.
    #[test]
    fn a_second_press_is_dropped_only_within_the_double_click_time() {
        let t0 = Instant::now();
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Waiting);
        assert!(book.skip_at(A, t0));
        assert!(!book.skip_at(A, t0), "the same instant");
        assert!(
            !book.skip_at(A, t0 + Duration::from_millis(1_999)),
            "a double click"
        );
        assert!(
            book.skip_at(A, t0 + DOUBLE_PRESS),
            "after the window a press sends again: the backend may have dropped the first"
        );
        // The window runs from the last SENT skip.
        assert!(!book.skip_at(A, t0 + DOUBLE_PRESS + Duration::from_millis(500)));
        // A report of the backend (a restart reports the state again) clears the guard.
        book.on_state(A, HistoryTransfer::Waiting);
        assert!(book.skip_at(A, t0 + DOUBLE_PRESS + Duration::from_millis(600)));
        // A clock that steps back never panics and counts as "just now".
        assert!(!book.skip_at(A, t0));
    }

    #[test]
    fn done_is_said_once_and_dismissed_at_every_report() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Reading);
        let end = book.on_state(A, HistoryTransfer::Done);
        assert_eq!(
            end,
            Outcome {
                changed: true,
                notice: Some(EndNotice::Transferred),
                dismiss: true
            }
        );
        assert!(
            book.view(A).is_none(),
            "nothing stays for a transfer that is done"
        );
        // The end is reported twice more (a connect before the dismissal arrived): no second
        // notice, and the dismissal is sent again for each report.
        for _ in 0..2 {
            assert_eq!(
                book.on_state(A, HistoryTransfer::Done),
                Outcome {
                    changed: false,
                    notice: None,
                    dismiss: true
                }
            );
        }
        assert_eq!(
            EndNotice::Transferred.text(),
            "The message history was transferred."
        );
    }

    #[test]
    fn an_end_found_at_the_start_is_said_once_too() {
        // The application starts long after the transfer ended; nothing was seen of it here.
        let mut book = TransferBook::default();
        let end = book.on_state(A, HistoryTransfer::Done);
        assert_eq!(end.notice, Some(EndNotice::Transferred));
        assert!(end.dismiss && !end.changed);
        assert_eq!(book.on_state(A, HistoryTransfer::Done).notice, None);
    }

    #[test]
    fn a_skip_of_our_own_ends_without_a_word() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Waiting);
        assert!(book.skip(A));
        // The state is what the backend says until it says otherwise.
        assert!(book.view(A).is_some());
        let end = book.on_state(A, HistoryTransfer::Skipped);
        assert_eq!(
            end,
            Outcome {
                changed: true,
                notice: None,
                dismiss: true
            }
        );
        assert!(book.view(A).is_none());
        assert_eq!(
            book.on_state(A, HistoryTransfer::Skipped),
            Outcome {
                changed: false,
                notice: None,
                dismiss: true
            }
        );
    }

    #[test]
    fn a_skip_by_the_phone_is_said_once() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Waiting);
        let end = book.on_state(A, HistoryTransfer::Skipped);
        assert_eq!(end.notice, Some(EndNotice::NotTransferred));
        assert!(end.dismiss && end.changed);
        let again = book.on_state(A, HistoryTransfer::Skipped);
        assert_eq!((again.notice, again.dismiss), (None, true));
        assert_eq!(
            EndNotice::NotTransferred.text(),
            "The message history was not transferred."
        );
    }

    #[test]
    fn a_skip_does_not_carry_over_to_the_next_transfer() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Waiting);
        assert!(book.skip(A));
        book.on_state(A, HistoryTransfer::Skipped);
        // The account is linked anew later and the phone declines this time.
        book.on_state(A, HistoryTransfer::Waiting);
        let end = book.on_state(A, HistoryTransfer::Skipped);
        assert_eq!(end.notice, Some(EndNotice::NotTransferred));
    }

    #[test]
    fn our_skip_that_came_too_late_still_reports_what_happened() {
        let mut book = TransferBook::default();
        book.on_state(A, downloading(9, Some(10)));
        assert!(book.skip(A));
        // The download finished first and the history was read.
        book.on_state(A, HistoryTransfer::Reading);
        let end = book.on_state(A, HistoryTransfer::Done);
        assert_eq!(end.notice, Some(EndNotice::Transferred));
    }

    #[test]
    fn a_failure_stays_until_it_is_dismissed_and_is_dismissed_once() {
        let mut book = TransferBook::default();
        book.on_state(A, downloading(1, None));
        let failed = HistoryTransfer::Failed(HistoryTransferFailure::Download);
        let end = book.on_state(A, failed);
        assert_eq!(
            end,
            Outcome {
                changed: true,
                notice: None,
                dismiss: false
            }
        );
        // Reported again at every connect: the banner stays, nothing is sent.
        for _ in 0..3 {
            assert_eq!(book.on_state(A, failed), Outcome::default());
        }
        let v = book.view(A).expect("the banner stays");
        assert_eq!(v.phase, TransferPhase::Failed);
        assert_eq!(v.retry, "Try Again…");
        assert_eq!(v.dismiss, "Dismiss");
        assert!(v.skip.is_empty());
        assert!(
            v.status
                .starts_with("The message history could not be downloaded.")
        );
        assert!(!book.running(A));
        // "Dismiss": once.
        assert!(book.dismiss(A));
        assert!(!book.dismiss(A), "a second press has nothing to dismiss");
        assert!(book.view(A).is_none());
        // The backend had a report on the way: no banner again, the dismissal once more.
        assert_eq!(
            book.on_state(A, failed),
            Outcome {
                changed: false,
                notice: None,
                dismiss: true
            }
        );
        assert!(book.view(A).is_none());
    }

    #[test]
    fn every_kind_of_failure_has_its_own_sentence_and_only_an_old_version_cannot_retry() {
        use HistoryTransferFailure::{
            Corrupt, Download, Incomplete, Internal, LinkAgain, TooLarge, UpdateApp,
        };
        let kinds = [
            LinkAgain, UpdateApp, Corrupt, TooLarge, Download, Internal, Incomplete,
        ];
        let mut texts = std::collections::HashSet::new();
        for kind in kinds {
            let mut book = TransferBook::default();
            book.on_state(A, HistoryTransfer::Failed(kind));
            let v = book.view(A).expect("shown");
            assert!(
                texts.insert(v.status.clone()),
                "{kind:?} repeats a sentence"
            );
            assert_eq!(v.retry.is_empty(), kind == UpdateApp, "{kind:?}");
            assert_eq!(v.dismiss, "Dismiss");
        }
        assert!(failure_text(UpdateApp).contains("Update Kourier first"));
        // A transfer that filled a part of the chats says that the part stays.
        assert!(failure_text(Incomplete).contains("Only part of the message history"));
    }

    #[test]
    fn a_new_failure_after_a_dismissed_one_shows_again() {
        let mut book = TransferBook::default();
        let download = HistoryTransfer::Failed(HistoryTransferFailure::Download);
        book.on_state(A, download);
        assert!(book.dismiss(A));
        // A new transfer (the account was linked anew under the same id) fails differently.
        book.on_state(A, HistoryTransfer::Waiting);
        let corrupt = HistoryTransfer::Failed(HistoryTransferFailure::Corrupt);
        assert!(book.on_state(A, corrupt).changed);
        assert!(book.view(A).is_some());
        // And the same failure as the dismissed one, after a transfer in between, too.
        assert!(book.dismiss(A));
        book.on_state(A, HistoryTransfer::Waiting);
        let again = book.on_state(A, corrupt);
        assert!(again.changed && !again.dismiss);
        assert!(book.view(A).is_some());
    }

    #[test]
    fn accounts_do_not_share_a_transfer_and_a_removed_account_shows_nothing() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Waiting);
        book.on_state(B, downloading(1, Some(2)));
        assert_eq!(book.view(A).map(|v| v.phase), Some(TransferPhase::Waiting));
        assert_eq!(
            book.view(B).map(|v| (v.account, v.phase)),
            Some((B, TransferPhase::Downloading))
        );
        assert!(book.skip(B));
        // B's skip is not A's.
        assert_eq!(
            book.on_state(A, HistoryTransfer::Skipped).notice,
            Some(EndNotice::NotTransferred)
        );
        assert!(book.remove(B), "it was shown");
        assert!(book.view(B).is_none() && !book.running(B) && !book.skip(B));
        assert!(!book.remove(B));
    }

    #[test]
    fn a_logout_forgets_a_running_transfer_and_keeps_an_end() {
        let mut book = TransferBook::default();
        // The backend deleted the transfer with the login and says nothing: a state that is
        // not over is gone here too, with our skip.
        for state in [
            HistoryTransfer::Waiting,
            downloading(1, Some(2)),
            HistoryTransfer::Reading,
        ] {
            book.on_state(A, state);
            book.skip(A);
            assert!(book.logged_out(A), "{state:?} was shown");
            assert!(book.view(A).is_none() && !book.running(A), "{state:?}");
            assert!(!book.logged_out(A), "nothing left to forget");
        }
        // A new transfer of the same account id (it was linked anew) starts clean: an end by
        // the phone is said.
        book.on_state(A, HistoryTransfer::Waiting);
        assert_eq!(
            book.on_state(A, HistoryTransfer::Skipped).notice,
            Some(EndNotice::NotTransferred)
        );
        // An end stays what it is: a failure nobody dismissed is still shown, and an end
        // that was said is not said again when it is reported once more.
        assert!(!book.logged_out(A));
        assert_eq!(
            book.on_state(A, HistoryTransfer::Skipped),
            Outcome {
                changed: false,
                notice: None,
                dismiss: true
            }
        );
        book.on_state(B, HistoryTransfer::Failed(HistoryTransferFailure::Corrupt));
        assert!(!book.logged_out(B));
        assert!(book.view(B).is_some());
    }

    #[test]
    fn an_account_is_told_by_its_name_and_its_number_or_messenger() {
        assert_eq!(
            account_line("Olha", "+380 44 123 4567", "Signal"),
            "Olha · +380 44 123 4567"
        );
        assert_eq!(account_line("Olha", "", "Signal"), "Olha · Signal");
        // Nothing is said twice, and nothing is added where there is nothing.
        assert_eq!(account_line("Signal", "", "Signal"), "Signal");
        assert_eq!(account_line("Olha", "Olha", "Signal"), "Olha");
        assert_eq!(account_line("Olha", "", ""), "Olha");
        assert_eq!(
            named_notice("Work", "The message history was transferred."),
            "Work: The message history was transferred."
        );
    }

    #[test]
    fn a_view_does_not_print_its_texts() {
        let mut book = TransferBook::default();
        book.on_state(A, HistoryTransfer::Waiting);
        let mut v = book.view(A).expect("shown");
        // (The banner of several accounts puts the account's name in front.)
        v.status = format!("Ada: {}", v.status);
        let printed = format!("{v:?}");
        assert!(printed.contains("Waiting") && !printed.contains("Ada"));
    }

    #[test]
    fn the_question_says_what_is_lost_and_names_its_buttons_by_what_they_do() {
        let q = replace_question(A, "Ada · +1 555 0100".into());
        assert_eq!(q.title, "Remove and Link Again?");
        assert_eq!(q.confirm, "Remove and Link Again");
        assert_eq!(q.cancel, "Cancel");
        let all = q.paragraphs.join(" ");
        for part in [
            "removes this account's chats, messages and files from this computer",
            "links the account again",
            "Your phone then asks whether to transfer",
            "exists only on this computer is lost",
            "older attachments may not be transferred",
            "the account stays removed",
            "You need your phone to link again",
        ] {
            assert!(all.contains(part), "missing: {part}");
        }
        assert!(
            !format!("{q:?}").contains("Ada"),
            "the account's name is not logged"
        );
        assert_eq!(transfer_action_label(), "Transfer History from Phone…");
    }
}
