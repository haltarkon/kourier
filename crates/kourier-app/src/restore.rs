//! Which chats a start reopens (split view S4): the stored `open-chat` (pane 0) and
//! `split-chat` (pane 1) against what exists and is shown now. A pure function so the rule
//! is tested without a window.
//!
//! "Exists and is shown" is the App's reading of the conversation list the window shows:
//! the chats of the SHOWN accounts, inbox and archive both (a chat of a hidden account, or one
//! the core no longer has, is not in it).

/// What a start opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// Nothing: no chat was stored, or none of them is usable now.
    Nothing,
    /// One chat in the first pane (no split): the stored first chat, or the stored second one
    /// when the first is gone (it opens as the only chat).
    One(i64),
    /// Both chats: the first in pane 0, the second beside it; pane 0 ends active.
    Both(i64, i64),
}

/// The choice. `open` and `split` are the stored ids (0 = none); `usable(id)` says that the
/// chat exists and its account is shown. The same chat is never in both panes.
pub fn plan(open: i64, split: i64, usable: impl Fn(i64) -> bool) -> Restore {
    let first = (open > 0 && usable(open)).then_some(open);
    let second = (split > 0 && split != open && usable(split)).then_some(split);
    match (first, second) {
        (Some(a), Some(b)) => Restore::Both(a, b),
        (Some(a), None) => Restore::One(a),
        (None, Some(b)) => Restore::One(b),
        (None, None) => Restore::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stored ids x what exists and is shown -> what opens.
    #[test]
    fn the_stored_chats_that_still_exist_reopen() {
        let usable = |id: i64| matches!(id, 4 | 7);
        // (stored open, stored split) -> plan
        let table = [
            ((4, 7), Restore::Both(4, 7)),
            ((4, 0), Restore::One(4)),
            ((0, 0), Restore::Nothing),
            // The first chat is gone: the second opens as the only chat.
            ((9, 7), Restore::One(7)),
            // The second is gone: the first opens alone.
            ((4, 9), Restore::One(4)),
            // Both gone.
            ((9, 8), Restore::Nothing),
            // Only a split was stored (a hand edit): it opens alone.
            ((0, 7), Restore::One(7)),
            // The same chat twice is one chat.
            ((4, 4), Restore::One(4)),
            // Nonsense ids are none.
            ((-4, -7), Restore::Nothing),
        ];
        for ((open, split), want) in table {
            assert_eq!(plan(open, split, usable), want, "{open} {split}");
        }
    }

    /// A chat of a hidden account is not usable: the caller's `usable` says so, and the other
    /// chat then opens alone.
    #[test]
    fn a_chat_of_a_hidden_account_is_skipped() {
        let shown = [4_i64];
        assert_eq!(plan(7, 4, |id| shown.contains(&id)), Restore::One(4));
        assert_eq!(plan(4, 7, |id| shown.contains(&id)), Restore::One(4));
    }
}
