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

use kourier_model::{AccountId, HistoryTransfer, HistoryTransferFailure};

use crate::i18n::{trc, trfc};
use crate::time::file_size;

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
}

impl TransferPhase {
    /// Stable name for QML (`waiting`, `downloading`, `reading`, `failed`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Downloading => "downloading",
            Self::Reading => "reading",
            Self::Failed => "failed",
        }
    }
}

/// One history transfer as it is shown: large on the link page (title, sentence, bar, bytes,
/// button) and compact in the account banner (one line, bytes, bar, button).
///
/// Its `status` can carry the name of an account once the banner tells several accounts
/// apart, so `Debug` prints the account's id and the phase only.
#[derive(Clone, PartialEq)]
pub struct TransferView {
    /// The account.
    pub account: AccountId,
    /// What it shows.
    pub phase: TransferPhase,
    /// The heading of the large presentation.
    pub title: String,
    /// The sentence under the heading; for a failure, what happened and what can be done.
    pub text: String,
    /// The one line of the compact presentation.
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
    /// The end in `state` was shown and dismissed: its notice went out (done, skipped), or
    /// the user closed its banner (failed). A repeated report of it shows nothing again.
    settled: bool,
}

/// The history transfers of all accounts, by what was last reported.
#[derive(Clone, Debug, Default)]
pub struct TransferBook {
    entries: HashMap<AccountId, Entry>,
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
    /// is until the backend reports the end.
    pub fn skip(&mut self, account: AccountId) -> bool {
        match self.entries.get_mut(&account) {
            Some(e) if e.state.can_skip() => {
                e.skipped_here = true;
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
        self.entries.remove(&account).is_some_and(|e| e.shows())
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
        Some(view(account, e.state))
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
        title: String::new(),
        text: String::new(),
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
            v.title = trc(
                "@title heading while the phone prepares the message history",
                "Waiting for Your Phone",
            )
            .into_owned();
            // (A transfer exists only when the phone's user already agreed to it: on Signal
            // the phone asks at the scan, before it links. While this waits the phone
            // prepares and uploads the history.)
            v.text = trc(
                "@info while the phone prepares the message history",
                "Your phone is preparing your message history. This can take a few minutes. New messages arrive once it is here.",
            )
            .into_owned();
            v.status = trc(
                "@info:status banner of an account",
                "Waiting for your phone to prepare the message history…",
            )
            .into_owned();
            v.skip = skip_label();
        }
        HistoryTransfer::Downloading { done, total } => {
            v.phase = TransferPhase::Downloading;
            v.title = trc(
                "@title heading while the message history downloads",
                "Downloading Message History",
            )
            .into_owned();
            v.text = trc(
                "@info while the message history downloads",
                "New messages arrive once the history is here.",
            )
            .into_owned();
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
            v.title = trc(
                "@title heading while the downloaded message history is read",
                "Importing Messages",
            )
            .into_owned();
            v.text = trc(
                "@info while the downloaded message history is read",
                "Keep Kourier open until this is finished. New messages arrive afterwards.",
            )
            .into_owned();
            v.status = trc(
                "@info:status banner of an account",
                "Importing messages… Keep Kourier open until this is finished.",
            )
            .into_owned();
        }
        HistoryTransfer::Failed(kind) => {
            v.phase = TransferPhase::Failed;
            // (No heading: a failure is one message in the banner.)
            v.text = failure_text(kind);
            v.status = v.text.clone();
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
        assert_eq!(v.title, "Waiting for Your Phone");
        assert_eq!(v.skip, "Continue Without History");
        assert_eq!(v.progress, -1.0);
        assert!(v.detail.is_empty() && v.retry.is_empty() && v.dismiss.is_empty());
        assert!(book.running(A));
    }

    #[test]
    fn a_download_says_its_bytes_and_copes_with_a_count_that_steps_back() {
        let mut book = TransferBook::default();
        let mb = 1_000_000;
        book.on_state(A, downloading(30 * mb, Some(40 * mb)));
        let v = book.view(A).expect("shown");
        assert_eq!(v.phase, TransferPhase::Downloading);
        assert_eq!(v.title, "Downloading Message History");
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
        assert_eq!(v.title, "Importing Messages");
        assert!(v.text.contains("Keep Kourier open"));
        assert!(v.skip.is_empty(), "no button to skip a read");
        assert_eq!(v.progress, -1.0);
        assert!(!book.skip(A), "and the skip is refused");
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
            v.text
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
            assert!(texts.insert(v.text.clone()), "{kind:?} repeats a sentence");
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
