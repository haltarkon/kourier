//! The group's page (people pages P2; architecture §8.3): what the details panel shows of one
//! group, built from the core's [`GroupDetail`]. Qt-free: the order of the members, the counts,
//! the cut, the rule lines, the timer's note and every text live here, the front end only
//! draws them.

use kourier_core::facade::GroupDetail;
use kourier_model::{
    AccountId, BlobRef, CannotSend, ConversationFeatures, ConversationId, MemberRole, UserId,
};

use crate::conversations::{
    TimerOption, TimerSpan, full_member_count, members_line, timer_menu, timer_secs, timer_span,
};
use crate::i18n::{trc, trfc, trnc};
use crate::names::{fallback_name, single_line};
use crate::preview;

/// How many members the page lists before "Show All" (Signal Desktop's 5).
pub const MEMBERS_PREVIEW: usize = 5;

/// The most characters of a group's description the page shows; the rest is cut at a character
/// border and ends in "…". The stored text stays whole. A limit of Kourier's panel (it protects it
/// from a member who sets a description of megabytes), not of the network; architecture §13.
pub const DESCRIPTION_LIMIT: usize = 4_096;
/// The other bound of the description: 16 KiB of UTF-8 (a hostile member's combining marks).
pub const DESCRIPTION_BYTES: usize = 16 * 1024;

/// How many of `total` rows a list shows before its "Show All" button: all of them, unless the
/// button would hide at least two (Signal Desktop's rule: a list of 6 shows 6, a list of 7 shows
/// 5 and "Show All (7)"). The members of a group and the groups two people share use this one
/// rule.
pub fn preview_rows(total: usize, preview: usize) -> usize {
    if total >= preview + 2 { preview } else { total }
}

/// One member of a group as the page lists them.
#[derive(Clone, PartialEq)]
pub struct GroupMemberRow {
    /// The member: what a click opens the page of.
    pub user: UserId,
    /// The name ("You" for the user's own row).
    pub name: String,
    /// The picture URL, empty for initials.
    pub avatar_url: String,
    /// An administrator (the row says so).
    pub admin: bool,
    /// The user's own row.
    pub is_self: bool,
}

crate::safe_debug!(GroupMemberRow { admin, is_self });

/// What the details panel shows for a group ([`crate::source::UiSource::take_group`]).
#[derive(Clone, PartialEq)]
pub struct GroupView {
    /// The account the group belongs to.
    pub account: AccountId,
    /// The group's conversation.
    pub id: ConversationId,
    /// The name ("Unnamed group" without one).
    pub name: String,
    /// The picture URL, empty for initials.
    pub avatar_url: String,
    /// The description (plain text, untrusted, line breaks kept, cut at the first of
    /// [`DESCRIPTION_LIMIT`] characters or [`DESCRIPTION_BYTES`] bytes, never inside an address);
    /// empty when there is none.
    pub description: String,
    /// The same text as whitelisted HTML for a selectable text (F1; [`crate::pagetext`]): escaped,
    /// a link around each address when [`Self::description_links`]; empty when there is none.
    pub description_html: String,
    /// Links are made in the description: the chat is accepted
    /// ([`crate::pagetext::links_allowed`]; a group that is only an invitation, a request or
    /// blocked shows plain text).
    pub description_links: bool,
    /// The addresses of the description's links, in order, each once (the keyboard's menu).
    pub description_hrefs: Vec<String>,
    /// Short lines under the name ("This group has ended."); usually none.
    pub notes: Vec<String>,
    /// The disappearing timer in seconds (0 = off).
    pub timer_secs: u32,
    /// The timer as the page says it: "1 week", or "Off".
    pub timer_label: String,
    /// The timers the group offers, "Off" first; empty when the timer cannot be changed here.
    pub timers: Vec<TimerOption>,
    /// The range of "Custom Time…"; `None`: only the offered values.
    pub timer_range: Option<TimerSpan>,
    /// Why the timer is read-only; empty when it can be changed here or when nothing needs saying
    /// (see [`timer_part`]).
    pub timer_note: String,
    /// Why the last change of the group's timer failed ("The disappearing-message timer could
    /// not be changed."), a line under the selector (F4); empty when none did. The source keeps
    /// it with the chat: the next change of any timer or the next page clears it.
    pub timer_failed: String,
    /// "6 members": the people in the group (members and admins, the user included).
    pub members_title: String,
    /// "2 invited"; empty when nobody is.
    pub invited: String,
    /// "1 request to join"; empty when nobody asked.
    pub requests: String,
    /// The word on an administrator's row ("Admin").
    pub admin_label: String,
    /// Every member and admin: the user first, then the admins, then the others, each by name.
    pub members: Vec<GroupMemberRow>,
    /// How many members the page lists (`members.len()`); it stays when the rows are taken out
    /// of the page ([`split_page`]), so a join or a leave always changes what is compared.
    pub member_count: usize,
    /// How many of `members` the group level lists; the `members` level lists all of them.
    pub members_preview: usize,
    /// "Show All (7)" when the group level hides members; empty otherwise.
    pub members_more: String,
    /// One line for each rule of the group that is on ("Only admins can add members."); none
    /// when the network stated no rules, none are on, or the group has ended.
    pub rules: Vec<String>,
}

crate::safe_debug!(GroupView {
    account,
    id,
    timer_secs,
    members_preview,
    member_count
});

/// What the timer part of the page does: a combo, or a read-only value with (maybe) a reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerPart {
    /// The user may pick a timer here.
    Combo,
    /// Read-only; the line under it (if any) says why.
    ReadOnly(Option<TimerNote>),
}

/// Why the timer is read-only on a group's page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerNote {
    /// The group's rules leave its info (and so the timer) to the admins and the user is none.
    /// Announcement-only plays no part: the timer follows the attributes access (Signal).
    OnlyAdmins,
    /// The network offers no timer to this client: it is changed on the phone.
    OnPhone,
}

/// The timer part of a group's page, as ONE table (tested as a table):
///
/// | the user's situation | part |
/// |---|---|
/// | not a member, invited, asked to join, group still loading, group ended | read-only, no line (the composer says why; an ended group is a note under the name) |
/// | only admins may change the group's settings, and the user is no admin (an announcement group alone does not count) | read-only, "Only admins can change this." |
/// | the network offers no timer | read-only, "Change this group's timer on your phone." |
/// | anything else | the combo |
///
/// A blocked group, a group whose request is pending and the like (`Blocked`, `RequestPending`,
/// `Unregistered`, `NotAccepted`, `NotYetBuilt`, `Other`) have no timer to change either, with no
/// line: the composer says why.
pub fn timer_part(features: &ConversationFeatures, we_admin: bool) -> TimerPart {
    let info = features.group_info.unwrap_or_default();
    match &features.cannot_send {
        Some(
            CannotSend::NotMember
            | CannotSend::InvitationPending
            | CannotSend::JoinRequestPending
            | CannotSend::StillLoading
            | CannotSend::Blocked
            | CannotSend::RequestPending
            | CannotSend::Unregistered
            | CannotSend::NotAccepted
            | CannotSend::NotYetBuilt
            | CannotSend::Other(_),
        ) => TimerPart::ReadOnly(None),
        _ if info.terminated => TimerPart::ReadOnly(None),
        _ => {
            // An announcement group (`AdminsOnly`) does not decide the timer: Signal's rule is the
            // attributes access alone (Signal-Android `canEditDisappearingMessages`).
            if info.attributes_admin_only && !we_admin {
                TimerPart::ReadOnly(Some(TimerNote::OnlyAdmins))
            } else if !features.has_timers() {
                TimerPart::ReadOnly(Some(TimerNote::OnPhone))
            } else {
                TimerPart::Combo
            }
        }
    }
}

/// Builds the page of `detail`. `avatar` turns a picture into its URL (empty for none).
pub fn group_view(detail: &GroupDetail, avatar: &dyn Fn(Option<&BlobRef>) -> String) -> GroupView {
    let clean = |s: &Option<String>| {
        s.as_deref()
            .map(single_line)
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
    };
    let name = clean(&detail.title).unwrap_or_else(|| {
        trc(
            "@item:intext title of a group without a name",
            "Unnamed group",
        )
        .into_owned()
    });
    let info = detail.features.group_info.unwrap_or_default();

    // The list holds full members and admins only: someone invited or asking to join is a count,
    // and the user's own invitation is no "You" row.
    let we_admin = detail
        .members
        .iter()
        .any(|m| m.is_self && m.role == MemberRole::Admin);
    let mut members: Vec<(u8, String, GroupMemberRow)> = detail
        .members
        .iter()
        .filter(|m| matches!(m.role, MemberRole::Member | MemberRole::Admin))
        .map(|m| {
            let admin = m.role == MemberRole::Admin;
            let shown = if m.is_self {
                trc("@item:intext a member row of a group, yourself", "You").into_owned()
            } else {
                clean(&m.name).unwrap_or_else(|| fallback_name(&m.user))
            };
            let rank = if m.is_self {
                0
            } else if admin {
                1
            } else {
                2
            };
            (
                rank,
                shown.to_lowercase(),
                GroupMemberRow {
                    user: m.user.clone(),
                    name: shown,
                    avatar_url: avatar(m.avatar.as_ref()),
                    admin,
                    is_self: m.is_self,
                },
            )
        })
        .collect();
    // You, then the admins, then everyone else, each by name the way a person reads a list; the
    // id keeps ties stable.
    members.sort_by(|a, b| (a.0, &a.1, a.2.user.as_str()).cmp(&(b.0, &b.1, b.2.user.as_str())));
    let members: Vec<GroupMemberRow> = members.into_iter().map(|(_, _, row)| row).collect();

    let full = full_member_count(detail.members.iter().map(|m| m.role));
    debug_assert_eq!(full, members.len());
    let count = |role| detail.members.iter().filter(|m| m.role == role).count() as u64;
    let (invited, asked) = (count(MemberRole::Pending), count(MemberRole::Requesting));
    let members_preview = preview_rows(members.len(), MEMBERS_PREVIEW);
    let members_more = if members_preview < members.len() {
        trfc(
            "@action:button show every member of a group, %1 how many",
            "Show All (%1)",
            &[&members.len().to_string()],
        )
    } else {
        String::new()
    };

    let secs = timer_secs(detail.timer);
    let (timers, timer_range, timer_note) = match timer_part(&detail.features, we_admin) {
        TimerPart::Combo => (
            timer_menu(&detail.features, secs),
            timer_span(&detail.features),
            String::new(),
        ),
        TimerPart::ReadOnly(note) => (
            Vec::new(),
            None,
            match note {
                None => String::new(),
                Some(TimerNote::OnlyAdmins) => trc(
                    "@info:status why a group setting is read-only",
                    "Only admins can change this.",
                )
                .into_owned(),
                Some(TimerNote::OnPhone) => trc(
                    "@info:status why the group's disappearing messages timer is read-only here",
                    "Change this group's timer on your phone.",
                )
                .into_owned(),
            },
        ),
    };

    let mut notes = Vec::new();
    let mut rules = Vec::new();
    if info.terminated {
        notes.push(
            trc(
                "@info:status a group that was ended for everyone",
                "This group has ended.",
            )
            .into_owned(),
        );
    } else {
        if info.attributes_admin_only {
            rules.push(
                trc(
                    "@info:status a group setting",
                    "Only admins can edit the group info.",
                )
                .into_owned(),
            );
        }
        if info.members_admin_only {
            rules.push(
                trc(
                    "@info:status a group setting",
                    "Only admins can add members.",
                )
                .into_owned(),
            );
        }
        if info.announcements_only {
            rules.push(
                trc(
                    "@info:status a group setting",
                    "Only admins can send messages.",
                )
                .into_owned(),
            );
        }
    }

    // The description's links follow the chat's reasons, not `cannot_send.is_some()` (F1, M1), and are off for a
    // blocked group: no backend states `CannotSend::Blocked`, the block is `conversations.blocked` (H1).
    let links =
        !detail.blocked && crate::pagetext::links_allowed(detail.features.cannot_send.as_ref());
    let described = crate::pagetext::page_text(detail.description.as_deref(), links);

    GroupView {
        account: detail.account,
        id: detail.id,
        name,
        avatar_url: avatar(detail.avatar.as_ref()),
        description: described.plain,
        description_html: described.html,
        description_links: links,
        description_hrefs: described.hrefs,
        notes,
        timer_secs: secs,
        timer_label: detail.timer.map(preview::timer_label).unwrap_or_else(|| {
            trc("@item:inlistbox disappearing messages timer", "Off").into_owned()
        }),
        timers,
        timer_range,
        timer_note,
        timer_failed: String::new(),
        members_title: members_line(full as u64),
        invited: if invited > 0 {
            trnc(
                "@info:status how many people are invited to a group, %1 the count",
                "%1 invited",
                "%1 invited",
                invited,
            )
        } else {
            String::new()
        },
        requests: if asked > 0 {
            trnc(
                "@info:status how many people asked to join a group, %1 the count",
                "%1 request to join",
                "%1 requests to join",
                asked,
            )
        } else {
            String::new()
        },
        admin_label: trc("@info:status a group member who is an admin", "Admin").into_owned(),
        member_count: members.len(),
        members,
        members_preview,
        members_more,
        rules,
    }
}

/// Whether a member row matches what was typed into the members search: the shown name contains
/// the typed text, whatever the case; the user's own row ("You") matches by that word. Spaces at
/// the ends of the typed text do not count; nothing typed matches everyone.
pub fn member_matches(name: &str, query: &str) -> bool {
    let query = query.trim();
    query.is_empty() || name.to_lowercase().contains(&query.to_lowercase())
}

/// Takes the member rows out of `view` and says whether the rest of the page must be put on screen:
/// `Some(rest)` when `rest` (the page without its rows, `member_count` included) differs from the
/// page `shown`, `None` when only member rows changed (the list models take those alone).
pub fn split_page(
    mut view: GroupView,
    shown: Option<&GroupView>,
) -> (Vec<GroupMemberRow>, Option<GroupView>) {
    let members = std::mem::take(&mut view.members);
    let changed = shown != Some(&view);
    (members, changed.then_some(view))
}

/// The role indices of a member row in a list model (`user` is role 0).
pub mod member_role {
    /// The member's user id (the row's key).
    pub const USER: u32 = 0;
    /// The shown name.
    pub const NAME: u32 = 1;
    /// The picture URL.
    pub const AVATAR_URL: u32 = 2;
    /// An administrator.
    pub const ADMIN: u32 = 3;
    /// The user's own row.
    pub const IS_SELF: u32 = 4;
}

impl GroupMemberRow {
    /// The row's identity in a list model: the member.
    pub fn key(&self) -> UserId {
        self.user.clone()
    }

    /// Which roles differ between two versions of the same member's row.
    pub fn changed_roles(a: &Self, b: &Self) -> crate::diff::RoleSet {
        use member_role::{ADMIN, AVATAR_URL, IS_SELF, NAME};
        crate::diff::RoleSet::EMPTY
            .with_if(NAME, a.name != b.name)
            .with_if(AVATAR_URL, a.avatar_url != b.avatar_url)
            .with_if(ADMIN, a.admin != b.admin)
            .with_if(IS_SELF, a.is_self != b.is_self)
    }
}

/// The rows of the members list for what was typed into its search: the members that match
/// ([`member_matches`]), in the page's order; everyone for an empty search.
pub fn listed_members(all: &[GroupMemberRow], query: &str) -> Vec<GroupMemberRow> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return all.to_vec();
    }
    all.iter()
        .filter(|m| m.name.to_lowercase().contains(&query))
        .cloned()
        .collect()
}

#[cfg(test)]
mod member_list_tests {
    use super::*;
    use crate::diff::{self, ListOp};

    fn row(user: &str, name: &str, admin: bool) -> GroupMemberRow {
        GroupMemberRow {
            user: UserId::parse(user).expect("an id"),
            name: name.to_owned(),
            avatar_url: String::new(),
            admin,
            is_self: false,
        }
    }

    fn id(n: u32) -> String {
        format!("aci:{n:08x}-0000-4000-8000-000000000000")
    }

    fn people() -> Vec<GroupMemberRow> {
        vec![
            row(&id(1), "Anna", true),
            row(&id(2), "Boris", false),
            row(&id(3), "Clara", false),
            row(&id(4), "Dmytro", false),
        ]
    }

    fn ops(old: &[GroupMemberRow], new: &[GroupMemberRow]) -> Vec<ListOp> {
        let ops = diff::diff(old, new, GroupMemberRow::key, GroupMemberRow::changed_roles);
        // Whatever the ops are, they must reproduce the new rows.
        let mut list = old.to_vec();
        diff::apply(&mut list, &ops, new);
        assert!(list == new, "the ops do not reproduce the rows");
        ops
    }

    #[test]
    fn a_rename_is_one_change_of_the_name_role() {
        let old = people();
        let mut new = old.clone();
        new[1].name = "Borys".into();
        let ops = ops(&old, &new);
        assert_eq!(ops.len(), 1);
        let ListOp::Change { at, roles } = ops[0] else {
            panic!("not a change: {ops:?}");
        };
        assert_eq!(at, 1);
        assert_eq!(roles.iter().collect::<Vec<_>>(), vec![member_role::NAME]);
    }

    #[test]
    fn a_join_is_one_insert_and_a_leave_one_remove() {
        let old = people();
        let mut new = old.clone();
        new.insert(2, row(&id(9), "Bohdan", false));
        assert_eq!(ops(&old, &new), vec![ListOp::Insert { at: 2, n: 1 }]);
        assert_eq!(ops(&new, &old), vec![ListOp::Remove { at: 2, n: 1 }]);
    }

    #[test]
    fn a_new_admin_is_a_move_and_a_change_of_the_admin_role() {
        let old = people();
        let mut new = old.clone();
        new[3].admin = true;
        let moved = new.remove(3);
        new.insert(1, moved);
        let ops = ops(&old, &new);
        assert!(
            ops.iter().any(|o| matches!(o, ListOp::Move { .. })),
            "no move: {ops:?}"
        );
        assert!(
            !ops.iter().any(|o| matches!(
                o,
                ListOp::Insert { .. } | ListOp::Remove { .. } | ListOp::Reset
            )),
            "the row was not kept: {ops:?}"
        );
        let changed: Vec<u32> = ops
            .iter()
            .filter_map(|o| match o {
                ListOp::Change { roles, .. } => Some(roles.iter().collect::<Vec<_>>()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(changed, vec![member_role::ADMIN]);
    }

    #[test]
    fn a_search_narrows_by_removes_and_widens_by_inserts() {
        let all = people();
        let everyone = listed_members(&all, "");
        assert_eq!(everyone.len(), 4);
        // "r" is in Boris, Clara, Dmytro, not in Anna... "Anna" has none.
        let narrow = listed_members(&all, " R ");
        let names: Vec<&str> = narrow.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Boris", "Clara", "Dmytro"]);
        assert_eq!(
            ops(&everyone, &narrow),
            vec![ListOp::Remove { at: 0, n: 1 }]
        );
        let widen = ops(&narrow, &everyone);
        assert_eq!(widen, vec![ListOp::Insert { at: 0, n: 1 }]);
        // Narrower still: only what matches stays, in the page's order.
        let one = listed_members(&all, "dm");
        assert_eq!(one.len(), 1);
        assert_eq!(ops(&narrow, &one), vec![ListOp::Remove { at: 0, n: 2 }]);
        assert!(listed_members(&all, "zzz").is_empty());
    }

    #[test]
    fn an_empty_page_resets_the_list_and_a_first_fill_too() {
        // What the model does (`diff_or_reset`): a side that is empty is ONE reset, not removes.
        let old = people();
        let ops = diff::diff_or_reset(
            &old,
            &[],
            GroupMemberRow::key,
            GroupMemberRow::changed_roles,
        );
        assert_eq!(ops, vec![ListOp::Reset]);
        let mut list = old.clone();
        diff::apply(&mut list, &ops, &[]);
        assert!(list.is_empty());
        let ops = diff::diff_or_reset(
            &[],
            &old,
            GroupMemberRow::key,
            GroupMemberRow::changed_roles,
        );
        assert_eq!(ops, vec![ListOp::Reset]);
        // A small change is not reset; `diff` alone would remove the rows one run.
        let by_hand = diff::diff(
            &old,
            &[],
            GroupMemberRow::key,
            GroupMemberRow::changed_roles,
        );
        assert_eq!(by_hand, vec![ListOp::Remove { at: 0, n: 4 }]);
        let mut fewer = old.clone();
        fewer.pop();
        let ops = diff::diff_or_reset(
            &old,
            &fewer,
            GroupMemberRow::key,
            GroupMemberRow::changed_roles,
        );
        assert_eq!(ops, vec![ListOp::Remove { at: 3, n: 1 }]);
    }

    #[test]
    fn the_search_is_the_same_rule_as_member_matches() {
        let all = people();
        for q in ["", "  ", "a", "AN", "bo", "x", " ris "] {
            let listed: Vec<String> = listed_members(&all, q)
                .into_iter()
                .map(|m| m.name)
                .collect();
            let want: Vec<String> = all
                .iter()
                .filter(|m| member_matches(&m.name, q))
                .map(|m| m.name.clone())
                .collect();
            assert_eq!(listed, want, "query {q:?}");
        }
    }
}
