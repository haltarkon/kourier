//! The share picker (people pages P5c, "Share Contact…"): the chats a contact card of the person
//! on screen can go to. Qt-free: which chats are listed, their order and the search live here;
//! the front end only draws the rows (`App.shareTargets`). The card itself is built by the core
//! (`UiCommand::SendContact`), never here.
//!
//! Listed: the chats of the person's account where the user can send, Note to Self included, the
//! archived ones too (after the inbox's, as the list's search shows them). Not listed: chats of
//! another account (a card never leaves through another account), chats whose features state a
//! `CannotSend` (not a member, an invitation or a join request pending, admins only, blocked, a
//! request pending, a channel the user cannot write), and chats the user deleted (hidden: the
//! sources never hold them).

use std::cmp::Ordering;

use kourier_model::{AccountId, ConversationId};

/// One chat the picker may list, before the account and the search narrow it.
#[derive(Clone, PartialEq, Eq)]
pub struct ShareTarget {
    /// The chat: what "Send" sends the card to.
    pub id: ConversationId,
    /// Its account.
    pub account: AccountId,
    /// Its title, as the conversation list shows it ("Note to Self" for the own chat).
    pub title: String,
    /// The picture URL, empty for initials.
    pub avatar_url: String,
    /// The chat with yourself.
    pub is_self: bool,
    /// The user can send here (no `CannotSend`).
    pub can_send: bool,
    /// In the archive.
    pub archived: bool,
    /// Pinned position, `None` when not pinned.
    pub pinned_order: Option<u32>,
    /// Last activity (ms), the order after pinning.
    pub last_activity: i64,
}

crate::safe_debug!(ShareTarget {
    id,
    account,
    is_self,
    can_send,
    archived,
    pinned_order,
    last_activity
});

/// Role indices of the share picker's list model (`App.shareTargets`).
pub mod target_role {
    /// The conversation id (the row's key).
    pub const CONVERSATION: u32 = 0;
    /// The title.
    pub const TITLE: u32 = 1;
    /// The picture URL.
    pub const AVATAR_URL: u32 = 2;
    /// The chat with yourself.
    pub const IS_SELF: u32 = 3;
}

impl ShareTarget {
    /// The row's identity in a list model: the chat.
    pub fn key(&self) -> ConversationId {
        self.id
    }

    /// Which roles differ between two versions of the same chat's row.
    pub fn changed_roles(a: &Self, b: &Self) -> crate::diff::RoleSet {
        use target_role::{AVATAR_URL, IS_SELF, TITLE};
        crate::diff::RoleSet::EMPTY
            .with_if(TITLE, a.title != b.title)
            .with_if(AVATAR_URL, a.avatar_url != b.avatar_url)
            .with_if(IS_SELF, a.is_self != b.is_self)
    }
}

/// The picker's rows: the chats of `all` a card of a person of `account` may go to (see the
/// module), whose title contains `query` (case-insensitive; every chat for an empty search),
/// in the list's order (pinned first by position, then the newest activity) with the archived
/// ones after the inbox's.
pub fn listed_targets(all: &[ShareTarget], account: AccountId, query: &str) -> Vec<ShareTarget> {
    let query = query.trim().to_lowercase();
    let mut out: Vec<ShareTarget> = all
        .iter()
        .filter(|t| t.account == account && t.can_send)
        .filter(|t| query.is_empty() || t.title.to_lowercase().contains(&query))
        .cloned()
        .collect();
    out.sort_by(|a, b| a.archived.cmp(&b.archived).then_with(|| list_order(a, b)));
    out
}

/// The conversation list's order ([`crate::conversations::compare_rows`]'s rule).
fn list_order(a: &ShareTarget, b: &ShareTarget) -> Ordering {
    crate::conversations::compare_order(
        (a.pinned_order, a.last_activity, a.id),
        (b.pinned_order, b.last_activity, b.id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: i64, account: i64, title: &str) -> ShareTarget {
        ShareTarget {
            id: ConversationId(id),
            account: AccountId(account),
            title: title.to_owned(),
            avatar_url: String::new(),
            is_self: false,
            can_send: true,
            archived: false,
            pinned_order: None,
            last_activity: id * 10,
        }
    }

    fn ids(rows: &[ShareTarget]) -> Vec<i64> {
        rows.iter().map(|t| t.id.0).collect()
    }

    #[test]
    fn share_targets_exclude_other_accounts_and_chats_that_cannot_send() {
        let mut no_send = target(3, 1, "Announcements");
        no_send.can_send = false;
        let mut note = target(4, 1, "Note to Self");
        note.is_self = true;
        let all = vec![
            target(1, 1, "Ada"),
            target(2, 2, "Ada at work"),
            no_send,
            note,
        ];
        // Newest first; another account's chat and a chat that cannot send are not listed; Note
        // to Self is.
        assert_eq!(ids(&listed_targets(&all, AccountId(1), "")), vec![4, 1]);
        assert_eq!(ids(&listed_targets(&all, AccountId(2), "")), vec![2]);
        assert!(listed_targets(&all, AccountId(9), "").is_empty());
    }

    #[test]
    fn share_targets_keep_the_lists_order_with_the_archive_after() {
        let mut pinned = target(1, 1, "Pinned");
        pinned.pinned_order = Some(0);
        let mut archived = target(9, 1, "Old group");
        archived.archived = true;
        let all = vec![target(5, 1, "Bob"), archived, pinned, target(7, 1, "Clara")];
        assert_eq!(
            ids(&listed_targets(&all, AccountId(1), "")),
            vec![1, 7, 5, 9]
        );
    }

    #[test]
    fn share_targets_search_titles_in_both_views() {
        let mut archived = target(9, 1, "Old Group");
        archived.archived = true;
        let all = vec![
            target(5, 1, "Bob"),
            archived,
            target(7, 1, "Group of friends"),
        ];
        assert_eq!(
            ids(&listed_targets(&all, AccountId(1), " GROUP ")),
            vec![7, 9]
        );
        assert!(listed_targets(&all, AccountId(1), "zzz").is_empty());
    }

    #[test]
    fn share_targets_announce_only_the_roles_that_changed() {
        let a = target(1, 1, "Ada");
        let mut b = a.clone();
        b.title = "Ada L.".into();
        b.last_activity = 99;
        assert_eq!(
            ShareTarget::changed_roles(&a, &b),
            crate::diff::RoleSet::EMPTY.with(target_role::TITLE)
        );
        assert_eq!(a.key(), b.key());
    }
}
