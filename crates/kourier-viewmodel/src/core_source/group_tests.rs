//! `CoreSource` and `group_view`: the group's page of the details panel (people pages P2),
//! against the simulated core of [`super::tests`].

use kourier_core::facade::{GroupDetail, GroupMember, Query, QueryData};
use kourier_model::{
    AccountId, AccountState, CannotSend, ConversationFeatures, ConversationId, GroupInfo, Member,
    MemberRole, UserId,
};

use super::tests::{Sent, Store, result, settle, source, take_queries, user};
use super::*;
use crate::group::{
    DESCRIPTION_BYTES, DESCRIPTION_LIMIT, GroupView, MEMBERS_PREVIEW, TimerNote, TimerPart,
    group_view, member_matches, preview_rows, split_page, timer_part,
};
use crate::source::GroupUpdate;

fn member(n: u32, role: MemberRole, name: Option<&str>) -> GroupMember {
    GroupMember {
        user: user(n),
        role,
        is_self: false,
        name: name.map(str::to_owned),
        avatar: None,
    }
}

fn mine(role: MemberRole) -> GroupMember {
    GroupMember {
        is_self: true,
        ..member(1, role, Some("My Own Profile Name"))
    }
}

fn detail(members: Vec<GroupMember>) -> GroupDetail {
    GroupDetail {
        account: AccountId(1),
        id: ConversationId(40),
        title: Some("Hiking".into()),
        avatar: None,
        description: None,
        timer: None,
        features: ConversationFeatures {
            group_info: Some(GroupInfo::default()),
            ..ConversationFeatures::default()
        },
        blocked: false,
        members,
        invites: kourier_model::GroupInvites::None,
    }
}

fn view(d: &GroupDetail) -> GroupView {
    group_view(d, &|_| String::new())
}

fn names(v: &GroupView) -> Vec<&str> {
    v.members.iter().map(|m| m.name.as_str()).collect()
}

/// The user's own row is first ("You", whatever the profile name), then the admins, then
/// everyone else, each group by name, case ignored; a tie is settled by the id.
#[test]
fn the_members_come_you_admins_others_each_by_name() {
    use MemberRole::{Admin, Member as M};
    let d = detail(vec![
        member(5, M, Some("zed")),
        member(6, Admin, Some("Yves")),
        member(7, M, Some("Adam")),
        mine(M),
        member(8, Admin, Some("bea")),
        member(9, M, Some("Same")),
        member(10, M, Some("same")),
    ]);
    let v = view(&d);
    assert_eq!(
        names(&v),
        ["You", "bea", "Yves", "Adam", "Same", "same", "zed"]
    );
    assert!(v.members[0].is_self && !v.members[0].admin);
    assert_eq!(
        v.members.iter().map(|m| m.admin).collect::<Vec<_>>(),
        [false, true, true, false, false, false, false]
    );
    // The two "same" rows keep the order of their ids.
    assert_eq!(v.members[4].user, user(9));
    // The user as an admin is still first.
    let mut d2 = d.clone();
    d2.members[3] = mine(Admin);
    let v2 = view(&d2);
    assert_eq!(
        (v2.members[0].name.as_str(), v2.members[0].admin),
        ("You", true)
    );
}

/// Someone invited or asking to join is a count, never a row; "N members" counts members and
/// admins only; the user's own invitation is no "You" row and counts as an invitation.
#[test]
fn people_invited_or_asking_are_counts_and_never_rows() {
    use MemberRole::{Admin, Member as M, Pending, Requesting};
    let d = detail(vec![
        mine(M),
        member(2, Admin, Some("Ann")),
        member(3, M, Some("Bob")),
        member(4, Pending, Some("Cy")),
        member(5, Pending, Some("Di")),
        member(6, Requesting, Some("Ed")),
    ]);
    let v = view(&d);
    assert_eq!(v.members_title, "3 members");
    assert_eq!(
        (v.invited.as_str(), v.requests.as_str()),
        ("2 invited", "1 request to join")
    );
    assert_eq!(names(&v), ["You", "Ann", "Bob"]);

    // Several asked.
    let mut many = d.clone();
    many.members.push(member(7, Requesting, None));
    assert_eq!(view(&many).requests, "2 requests to join");

    // Nobody invited, nobody asking: no line.
    let calm = view(&detail(vec![mine(M), member(2, M, Some("Ann"))]));
    assert_eq!(
        (
            calm.invited.as_str(),
            calm.requests.as_str(),
            calm.members_title.as_str()
        ),
        ("", "", "2 members")
    );

    // We are invited: no "You" row, "1 invited" includes us, the others are listed.
    let invited = view(&detail(vec![
        mine(Pending),
        member(2, Admin, Some("Ann")),
        member(3, M, Some("Bob")),
    ]));
    assert_eq!(names(&invited), ["Ann", "Bob"]);
    assert_eq!(
        (invited.members_title.as_str(), invited.invited.as_str()),
        ("2 members", "1 invited")
    );
    // We asked to join: the same, as a request.
    let asked = view(&detail(vec![mine(Requesting), member(2, M, Some("Ann"))]));
    assert_eq!(names(&asked), ["Ann"]);
    assert_eq!(asked.requests, "1 request to join");

    // We left (no row of ours): the others are listed, nobody is "You".
    let left = view(&detail(vec![
        member(2, Admin, Some("Ann")),
        member(3, M, None),
    ]));
    assert!(left.members.iter().all(|m| !m.is_self && m.name != "You"));
    assert_eq!(left.members.len(), 2);
}

/// A group that has ended keeps its member rows under "Members", says so under the name, and
/// shows no rule: nobody can change anything.
#[test]
fn an_ended_group_keeps_its_rows_and_says_so() {
    let mut d = detail(vec![
        mine(MemberRole::Member),
        member(2, MemberRole::Admin, Some("Ann")),
    ]);
    d.features.group_info = Some(GroupInfo {
        attributes_admin_only: true,
        members_admin_only: true,
        announcements_only: true,
        terminated: true,
    });
    d.features.cannot_send = Some(CannotSend::NotMember);
    let v = view(&d);
    assert_eq!(v.notes, ["This group has ended."]);
    assert_eq!(names(&v), ["You", "Ann"]);
    assert!(v.rules.is_empty(), "{:?}", v.rules);
    assert_eq!(v.timer_note, "");
    assert!(v.timers.is_empty());
    // A group that did not end has no note.
    assert!(view(&detail(Vec::new())).notes.is_empty());
}

/// One line for each rule that is on, in a fixed order; none when the network stated no rules.
#[test]
fn the_rules_are_one_line_each_for_the_rules_that_are_on() {
    let mut d = detail(Vec::new());
    let rules = |d: &GroupDetail| view(d).rules;
    assert!(rules(&d).is_empty(), "all off: nothing to say");
    d.features.group_info = Some(GroupInfo {
        attributes_admin_only: true,
        ..GroupInfo::default()
    });
    assert_eq!(rules(&d), ["Only admins can edit the group info."]);
    d.features.group_info = Some(GroupInfo {
        members_admin_only: true,
        ..GroupInfo::default()
    });
    assert_eq!(rules(&d), ["Only admins can add members."]);
    d.features.group_info = Some(GroupInfo {
        announcements_only: true,
        ..GroupInfo::default()
    });
    assert_eq!(rules(&d), ["Only admins can send messages."]);
    d.features.group_info = Some(GroupInfo {
        attributes_admin_only: true,
        members_admin_only: true,
        announcements_only: true,
        terminated: false,
    });
    assert_eq!(
        rules(&d),
        [
            "Only admins can edit the group info.",
            "Only admins can add members.",
            "Only admins can send messages."
        ]
    );
    // A network that stated nothing: no claim either way.
    d.features.group_info = None;
    assert!(rules(&d).is_empty());
}

/// The timer part is one table: who may change it, and what the line under it says.
#[test]
fn the_timer_part_follows_one_table() {
    use CannotSend::{
        AdminsOnly, Blocked, InvitationPending, JoinRequestPending, NotAccepted, NotMember,
        NotYetBuilt, Other, RequestPending, StillLoading, Unregistered,
    };
    use TimerNote::{OnPhone, OnlyAdmins};
    use TimerPart::{Combo, ReadOnly};
    let timers = || {
        let mut f = ConversationFeatures::default();
        f.disappearing
            .insert(kourier_model::DisappearKind::AfterSend, vec![3600, 86_400]);
        f
    };
    let none = ConversationFeatures::default();
    let info = |a, m, n, t| {
        Some(GroupInfo {
            attributes_admin_only: a,
            members_admin_only: m,
            announcements_only: n,
            terminated: t,
        })
    };
    let with = |mut f: ConversationFeatures, cannot: Option<CannotSend>, gi| {
        f.cannot_send = cannot;
        f.group_info = gi;
        f
    };
    // (features, we are admin) -> part
    let table: Vec<(&str, ConversationFeatures, bool, TimerPart)> = vec![
        // The network offers no timer here: on the phone.
        (
            "no timer",
            with(none.clone(), None, info(false, false, false, false)),
            false,
            ReadOnly(Some(OnPhone)),
        ),
        (
            "no timer, no rules stated",
            with(none.clone(), None, None),
            false,
            ReadOnly(Some(OnPhone)),
        ),
        // Admin-only settings and no admin: only admins, with or without a timer on offer.
        (
            "admin-only, not admin, no timer",
            with(none.clone(), None, info(true, false, false, false)),
            false,
            ReadOnly(Some(OnlyAdmins)),
        ),
        (
            "admin-only, not admin, timers",
            with(timers(), None, info(true, false, false, false)),
            false,
            ReadOnly(Some(OnlyAdmins)),
        ),
        // An admin of an admin-only group is as good as anyone elsewhere.
        (
            "admin-only, admin, timers",
            with(timers(), None, info(true, false, false, false)),
            true,
            Combo,
        ),
        (
            "admin-only, admin, no timer",
            with(none.clone(), None, info(true, false, false, false)),
            true,
            ReadOnly(Some(OnPhone)),
        ),
        // Only the rule on adding members or announcements does not touch the timer.
        (
            "members-only rule, timers",
            with(timers(), None, info(false, true, false, false)),
            false,
            Combo,
        ),
        // Announcement group, not an admin: sending is off.
        (
            "announcements, not admin, timers",
            with(timers(), Some(AdminsOnly), info(false, false, true, false)),
            false,
            Combo,
        ),
        (
            "announcements, not admin, no timer",
            with(
                none.clone(),
                Some(AdminsOnly),
                info(false, false, true, false),
            ),
            false,
            ReadOnly(Some(OnPhone)),
        ),
        // Announcement group with admin-only info, not an admin: the info rule speaks.
        (
            "announcements, admin-only, not admin, timers",
            with(timers(), Some(AdminsOnly), info(true, false, true, false)),
            false,
            ReadOnly(Some(OnlyAdmins)),
        ),
        // Plain offered timers: a combo.
        (
            "timers on offer",
            with(timers(), None, info(false, false, false, false)),
            false,
            Combo,
        ),
        (
            "timers on offer, rules not stated",
            with(timers(), None, None),
            false,
            Combo,
        ),
        // Not in the group (yet or any more): the composer says why, the page says nothing.
        (
            "not a member",
            with(timers(), Some(NotMember), info(false, false, false, false)),
            false,
            ReadOnly(None),
        ),
        (
            "invited",
            with(
                none.clone(),
                Some(InvitationPending),
                info(false, false, false, false),
            ),
            false,
            ReadOnly(None),
        ),
        (
            "asked to join",
            with(
                none.clone(),
                Some(JoinRequestPending),
                info(false, false, false, false),
            ),
            false,
            ReadOnly(None),
        ),
        (
            "still loading",
            with(none.clone(), Some(StillLoading), None),
            false,
            ReadOnly(None),
        ),
        (
            "ended",
            with(timers(), None, info(false, false, false, true)),
            false,
            ReadOnly(None),
        ),
        // Ended while still a member, the info admin-only and we no admin: still no line (F7).
        (
            "ended, member, admin-only, not admin",
            with(timers(), None, info(true, false, false, true)),
            false,
            ReadOnly(None),
        ),
        (
            "ended and not a member",
            with(
                none.clone(),
                Some(NotMember),
                info(true, false, false, true),
            ),
            false,
            ReadOnly(None),
        ),
        (
            "blocked",
            with(timers(), Some(Blocked), None),
            false,
            ReadOnly(None),
        ),
        (
            "request pending",
            with(timers(), Some(RequestPending), None),
            false,
            ReadOnly(None),
        ),
        (
            "unregistered",
            with(timers(), Some(Unregistered), None),
            false,
            ReadOnly(None),
        ),
        (
            "not accepted",
            with(timers(), Some(NotAccepted), None),
            false,
            ReadOnly(None),
        ),
        (
            "not yet built",
            with(timers(), Some(NotYetBuilt), None),
            false,
            ReadOnly(None),
        ),
        (
            "another reason",
            with(timers(), Some(Other("x".into())), None),
            false,
            ReadOnly(None),
        ),
    ];
    for (what, features, admin, part) in &table {
        assert_eq!(timer_part(features, *admin), *part, "{what}");
    }

    // The words of the two lines, through the view.
    let mut d = detail(vec![mine(MemberRole::Member)]);
    d.features = with(none, None, info(false, false, false, false));
    assert_eq!(
        view(&d).timer_note,
        "Change this group's timer on your phone."
    );
    d.features.group_info = info(true, false, false, false);
    assert_eq!(view(&d).timer_note, "Only admins can change this.");
    d.features = with(timers(), None, info(false, false, false, false));
    let v = view(&d);
    assert_eq!(v.timer_note, "");
    assert_eq!(v.timers.first().map(|t| t.secs), Some(0), "Off first");
    assert!(v.timers.len() >= 3);
    d.timer = Some(std::time::Duration::from_secs(3600));
    let v = view(&d);
    assert_eq!((v.timer_secs, v.timer_label.as_str()), (3600, "1 hour"));
    d.timer = None;
    assert_eq!(view(&d).timer_label, "Off");
}

/// The description's links follow the chat's reason, one row for every variant of `CannotSend`
/// (critic M1; mutant: `cannot_send.is_none()`, which takes the links from an announcement group and
/// from every Threema and Telegram chat). The text is the same either way.
#[test]
fn the_description_links_unless_the_group_is_an_invitation_a_request_or_blocked() {
    let text = "Meet at https://example.org/a. Rules: example.net/b";
    let reasons = [
        (None, true),
        (Some(CannotSend::NotMember), true),
        (Some(CannotSend::AdminsOnly), true),
        (Some(CannotSend::Unregistered), true),
        (Some(CannotSend::StillLoading), true),
        // The Threema and Telegram chats until their send path exists, on the fake playing either.
        (Some(CannotSend::NotYetBuilt), true),
        (Some(CannotSend::Other("another reason".into())), true),
        (Some(CannotSend::Blocked), false),
        (Some(CannotSend::RequestPending), false),
        (Some(CannotSend::NotAccepted), false),
        (Some(CannotSend::InvitationPending), false),
        (Some(CannotSend::JoinRequestPending), false),
    ];
    for (reason, linked) in reasons {
        let mut d = detail(Vec::new());
        d.features.cannot_send = reason.clone();
        d.description = Some(text.into());
        let v = view(&d);
        assert_eq!(v.description_links, linked, "reason {reason:?}");
        assert_eq!(
            v.description_html
                .contains("<a href=\"https://example.org/a\">"),
            linked,
            "reason {reason:?}: {}",
            v.description_html
        );
        let want: &[&str] = if linked {
            &["https://example.org/a", "https://example.net/b"]
        } else {
            &[]
        };
        assert_eq!(v.description_hrefs, want, "reason {reason:?}");
        assert_eq!(
            v.description, text,
            "the text is the same: reason {reason:?}"
        );
    }
    // H1: no backend states `CannotSend::Blocked`; a blocked group is `conversations.blocked`, with any reason, none
    // included: plain text.
    for reason in [None, Some(CannotSend::AdminsOnly)] {
        let mut d = detail(Vec::new());
        d.features.cannot_send = reason.clone();
        d.blocked = true;
        d.description = Some(text.into());
        let v = view(&d);
        assert!(
            !v.description_links
                && v.description_hrefs.is_empty()
                && !v.description_html.contains("<a "),
            "a blocked group, reason {reason:?}: {}",
            v.description_html
        );
        assert_eq!(
            v.description, text,
            "the text is the same: blocked, {reason:?}"
        );
    }
}

/// The page's HTML is the description escaped (markup is text), empty for no description, and cut
/// like the plain text, ending in "…".
#[test]
fn the_description_html_is_the_escaped_text_and_the_cut_one() {
    let mut d = detail(Vec::new());
    assert_eq!(view(&d).description_html, "");
    d.description = Some("A <b>bold</b> & more\nSecond line".into());
    let v = view(&d);
    assert_eq!(
        v.description_html,
        "A &lt;b&gt;bold&lt;/b&gt; &amp; more<br/>Second line"
    );
    d.description = Some(format!("{}c", "ab".repeat(DESCRIPTION_LIMIT / 2)));
    let v = view(&d);
    assert!(v.description.ends_with('…') && v.description_html.ends_with('…'));
    assert_eq!(v.description_html.matches('…').count(), 1);
}

/// The description keeps its line breaks, is trimmed, and is cut at a character border at the
/// limit; the cut ends in "…" and a text of exactly the limit is whole.
#[test]
fn the_description_keeps_its_lines_is_trimmed_and_cut_at_the_limit() {
    let mut d = detail(Vec::new());
    d.description = Some("  Meet at six.\r\nBring water.\n\nBye  \n".into());
    assert_eq!(view(&d).description, "Meet at six.\nBring water.\n\nBye");
    for none in [None, Some(String::new()), Some("  \n ".to_owned())] {
        d.description = none;
        assert_eq!(view(&d).description, "");
    }

    let whole: String = "ab".repeat(DESCRIPTION_LIMIT / 2);
    d.description = Some(whole.clone());
    assert_eq!(view(&d).description, whole, "exactly the limit: whole");
    d.description = Some(format!("{whole}c"));
    let cut = view(&d).description;
    assert_eq!(cut.chars().count(), DESCRIPTION_LIMIT + 1);
    assert!(cut.ends_with('…') && cut.starts_with("abab"));

    // Characters are counted as a person counts them: a flag and an accent made of two code
    // points are ONE character each, and a cut never lands inside one.
    let flag = "🇺🇦";
    let accent = "e\u{301}";
    d.description = Some(format!(
        "{}{flag}{accent}{flag}",
        "x".repeat(DESCRIPTION_LIMIT - 2)
    ));
    let kept = view(&d).description;
    assert_eq!(
        kept,
        format!("{}{flag}{accent}…", "x".repeat(DESCRIPTION_LIMIT - 2)),
        "the limit ends after the accent, the last flag is cut whole"
    );
    // The stored text is not touched by the cut (the view only shows).
    assert_eq!(
        d.description.as_deref().map(|t| t.chars().count()),
        Some(DESCRIPTION_LIMIT - 2 + 2 + 2 + 2)
    );
}

/// One grapheme can be megabytes (a letter and 100,000 combining marks): the byte ceiling cuts.
#[test]
fn a_description_of_one_huge_grapheme_is_cut_by_bytes() {
    let mut d = detail(Vec::new());
    d.description = Some(format!("a{}", "\u{301}".repeat(100_000)));
    let cut = view(&d).description;
    assert!(cut.ends_with('…'));
    assert!(
        cut.len() <= 16 * 1024 + '…'.len_utf8(),
        "{} bytes",
        cut.len()
    );
    assert!(cut.starts_with('a'));
    assert!(
        cut.len() > 16 * 1024 - 4,
        "cut at the last char boundary, not earlier: {} bytes",
        cut.len()
    );
}

/// The byte bound's edges: 16 KiB exactly stays whole, one byte more is cut; a character that
/// would cross the bound goes whole (a 4-byte one, and a family emoji of 18 bytes: never half of
/// it before the "…").
#[test]
fn the_byte_bound_keeps_whole_characters_at_its_edge() {
    let mut d = detail(Vec::new());
    // 16 KiB of 4-byte characters is 4,096 of them: within both bounds, whole.
    let whole = "😀".repeat(DESCRIPTION_BYTES / 4);
    assert_eq!(whole.len(), DESCRIPTION_BYTES);
    d.description = Some(whole.clone());
    assert_eq!(view(&d).description, whole, "exactly 16 KiB: whole");
    // Over 16 KiB in few characters (910 families of 18 bytes, "ab", then a 4-byte emoji that
    // would end at 16,386): cut by bytes, the emoji goes whole.
    let family = "👨\u{200d}👩\u{200d}👧";
    let head = format!("{}ab", family.repeat(910));
    assert_eq!(head.len(), DESCRIPTION_BYTES - 2);
    d.description = Some(format!("{head}😀"));
    assert_eq!(
        view(&d).description,
        format!("{head}…"),
        "the 4-byte character crossing the bound goes whole"
    );
    // Family emoji (one character of 18 bytes): the cut lands before one, never inside.
    assert_eq!(family.len(), 18);
    d.description = Some(family.repeat(1_000));
    let cut = view(&d).description;
    let kept = cut.strip_suffix('…').expect("cut");
    assert_eq!(
        kept.len() % 18,
        0,
        "whole families only: {} bytes",
        kept.len()
    );
    assert_eq!(kept.len(), DESCRIPTION_BYTES / 18 * 18);
}

/// The member rows are cut at five, with "Show All (N)" only when that hides two or more: a
/// group of 6 shows all 6, a group of 7 shows 5 and "Show All (7)".
#[test]
fn the_members_are_cut_at_five_with_show_all_from_two_hidden() {
    assert_eq!(MEMBERS_PREVIEW, 5);
    for (total, preview, more) in [
        (0usize, 0usize, ""),
        (4, 4, ""),
        (5, 5, ""),
        (6, 6, ""),
        (7, 5, "Show All (7)"),
        (30, 5, "Show All (30)"),
    ] {
        let d = detail(
            (0..total)
                .map(|i| member(10 + i as u32, MemberRole::Member, Some(&format!("M{i:02}"))))
                .collect(),
        );
        let v = view(&d);
        assert_eq!(
            (v.members_preview, v.members_more.as_str(), v.members.len()),
            (preview, more, total),
            "{total} members"
        );
    }
    // Invited people never count toward the cut.
    let mut d = detail(
        (0..5)
            .map(|i| member(10 + i, MemberRole::Member, None))
            .collect(),
    );
    d.members.push(member(30, MemberRole::Pending, None));
    d.members.push(member(31, MemberRole::Requesting, None));
    assert_eq!(view(&d).members_more, "");
    // The one rule, as a function.
    assert_eq!(
        [preview_rows(6, 5), preview_rows(7, 5), preview_rows(2, 5)],
        [6, 5, 2]
    );
}

/// A member without a contact name gets a label made of the id (never the id), a member with
/// one gets it on a single line; nothing personal reaches `Debug`.
#[test]
fn a_member_without_a_name_gets_a_label_and_debug_shows_no_names() {
    use MemberRole::Member as M;
    let mut d = detail(vec![
        member(2, M, None),
        member(3, M, Some("  Cleo\nSecret  ")),
        member(4, M, Some("   ")),
        mine(M),
    ]);
    d.description = Some("Secret description".into());
    let v = view(&d);
    assert!(
        v.members.iter().any(|m| m.name == "Cleo Secret"),
        "{:?}",
        names(&v)
    );
    let labelled: Vec<&str> = v
        .members
        .iter()
        .filter(|m| m.name.contains("Signal user"))
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(labelled.len(), 2, "{:?}", names(&v));
    assert!(v.members.iter().all(|m| !m.name.contains("00000000-0000")));
    let shown = format!(
        "{v:?} {d:?} {:?} {:?}",
        v.members[0],
        GroupUpdate::Show(Box::new(v.clone()))
    );
    for leak in ["Secret", "Hiking", "Cleo", "My Own Profile"] {
        assert!(!shown.contains(leak), "{leak} in {shown}");
    }
    // A group without a name.
    d.title = None;
    assert_eq!(view(&d).name, "Unnamed group");
}

/// The search of "Show All": the shown name contains what was typed, case ignored; nothing
/// typed matches everyone; the user's own row matches by "You".
#[test]
fn the_search_matches_the_shown_name_whatever_the_case() {
    assert!(member_matches("Alice Arden", "ard"));
    assert!(member_matches("Alice Arden", "  ALICE "));
    assert!(member_matches("Alice Arden", ""));
    assert!(member_matches("Alice Arden", "   "));
    assert!(!member_matches("Alice Arden", "bob"));
    let d = detail(vec![
        mine(MemberRole::Member),
        member(2, MemberRole::Member, Some("Bob")),
    ]);
    let v = view(&d);
    let hits: Vec<&str> = v
        .members
        .iter()
        .filter(|m| member_matches(&m.name, "you"))
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(hits, ["You"]);
}

// ---- CoreSource -------------------------------------------------------------------------------

fn opened_source(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    settle(&mut src, &sent, store);
    (src, sent)
}

/// How many group queries wait in `sent` (they stay there for [`answer`]).
fn group_queries(sent: &Sent) -> usize {
    sent.borrow()
        .iter()
        .filter(|c| {
            matches!(
                c,
                UiCommand::Query {
                    query: Query::Group(_),
                    ..
                }
            )
        })
        .count()
}

fn answer(src: &mut CoreSource, sent: &Sent, detail: Option<GroupDetail>) -> Changes {
    let asked = take_queries(sent);
    let (token, query) = asked
        .iter()
        .rev()
        .find(|(_, q)| matches!(q, Query::Group(_)))
        .cloned()
        .expect("a group query");
    assert_eq!(query, Query::Group(ConversationId(40)));
    src.on_event(&result(token, QueryData::Group(detail.map(Box::new))))
}

/// Answers the group query waiting in `sent` (see [`group_queries`], which takes queries out).
fn answer_with(src: &mut CoreSource, sent: &Sent, detail: GroupDetail) -> Changes {
    answer(src, sent, Some(detail))
}

fn shown(src: &mut CoreSource, sent: &Sent) {
    assert!(src.request_group(AccountId(1), ConversationId(40)));
    answer(src, sent, Some(detail(vec![mine(MemberRole::Member)])));
    assert!(matches!(src.take_group(), Some(GroupUpdate::Show(_))));
}

/// F4 (critic M6): the group's page says why the last change of ITS timer failed (the same rule as the
/// person's page): the failure of another chat is not its own, a refresh carries it on, the next change of a
/// timer forgets it, and a new ask starts without it. The member rows play no part: the failure is in the
/// page the front end compares (`split_page`), so a failure alone puts `App.group` on screen again.
#[test]
fn a_failed_timer_change_is_said_on_the_groups_page_until_the_next_change_or_page() {
    use kourier_core::facade::ActionSubject;
    let network_no = || -> kourier_core::facade::CoreError {
        kourier_model::BackendError::new(kourier_model::ErrorKind::InvalidInput, "refused").into()
    };
    let said = "The disappearing-message timer could not be changed.";
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened_source(&store);
    shown(&mut src, &sent);
    let subject = ActionSubject::Conversation(ConversationId(40));
    let ch = src.on_action_failed(
        "SetConversationTimer",
        &network_no(),
        &ActionSubject::Conversation(ConversationId(2)),
    );
    assert!(ch.notice && !ch.group && !ch.person);
    assert!(src.take_group().is_none());

    let ch = src.on_action_failed("SetConversationTimer", &network_no(), &subject);
    assert!(ch.group && ch.notice);
    let Some(GroupUpdate::Show(v)) = src.take_group() else {
        panic!("the page, with the failure");
    };
    assert_eq!(v.timer_failed, said);
    // The failure alone is a change of the page the front end puts on screen.
    let mut on_screen = (*v).clone();
    on_screen.members.clear();
    on_screen.timer_failed.clear();
    let (_, rest) = split_page(*v, Some(&on_screen));
    assert!(rest.is_some_and(|r| r.timer_failed == said));

    // A refresh (a member joined) carries it on.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(40)],
    });
    let ch = answer(
        &mut src,
        &sent,
        Some(detail(vec![
            mine(MemberRole::Member),
            member(2, MemberRole::Member, Some("Ann")),
        ])),
    );
    assert!(ch.group);
    let Some(GroupUpdate::Show(v)) = src.take_group() else {
        panic!("the page, refreshed");
    };
    assert_eq!((v.members.len(), v.timer_failed.as_str()), (2, said));

    // The next change forgets it.
    assert!(src.set_timer(ConversationId(40), 3600));
    let Some(GroupUpdate::Show(v)) = src.take_group() else {
        panic!("the page, without the failure");
    };
    assert_eq!(v.timer_failed, "");

    // A new ask starts without it.
    src.on_action_failed("SetConversationTimer", &network_no(), &subject);
    assert!(src.take_group().is_some());
    assert!(src.request_group(AccountId(1), ConversationId(40)));
    answer(
        &mut src,
        &sent,
        Some(detail(vec![mine(MemberRole::Member)])),
    );
    let Some(GroupUpdate::Show(v)) = src.take_group() else {
        panic!("the page again");
    };
    assert_eq!(v.timer_failed, "", "the next page does not carry it");
}

/// Opening a page asks for the group; the changes that can move it (its own row, a contact)
/// read it again, coalesced; a message in another chat does not.
#[test]
fn opening_a_group_page_asks_the_core_and_only_its_own_changes_read_it_again() {
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened_source(&store);
    assert!(src.request_group(AccountId(1), ConversationId(40)));
    let mut d = detail(vec![
        mine(MemberRole::Member),
        member(2, MemberRole::Admin, Some("Ann")),
    ]);
    let ch = answer(&mut src, &sent, Some(d.clone()));
    assert!(ch.group && !ch.person);
    let Some(GroupUpdate::Show(v)) = src.take_group() else {
        panic!("a page");
    };
    assert_eq!(v.name, "Hiking");
    assert!(src.take_group().is_none(), "taken once");

    // The group changed: read again, even many times in a row, one query in flight and one
    // follow-up for all the changes that came meanwhile.
    for _ in 0..3 {
        src.on_event(&UiEvent::ConversationsChanged {
            ids: vec![ConversationId(40), ConversationId(2)],
        });
    }
    assert_eq!(group_queries(&sent), 1, "coalesced");
    let ch = answer_with(&mut src, &sent, d.clone());
    assert!(!ch.group, "the same page again");
    assert_eq!(
        group_queries(&sent),
        1,
        "one follow-up for the three changes"
    );
    answer_with(&mut src, &sent, d.clone());
    assert_eq!(group_queries(&sent), 0, "and then it is quiet");
    // Another chat changed (a message): no read.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(2)],
    });
    assert_eq!(group_queries(&sent), 0, "an unrelated chat asks nothing");
    // A contact changed (a member renamed): read again.
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    d.members[1].name = Some("Anna".into());
    let ch = answer(&mut src, &sent, Some(d));
    assert!(ch.group);
    let Some(GroupUpdate::Show(v)) = src.take_group() else {
        panic!("a page");
    };
    assert!(v.members.iter().any(|m| m.name == "Anna"));
}

/// An answer equal to the page on screen changes nothing; a real change is handed over; the
/// first answer of a new ask always is.
#[test]
fn a_group_answer_equal_to_the_page_on_screen_changes_nothing() {
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened_source(&store);
    let d = detail(vec![
        mine(MemberRole::Member),
        member(2, MemberRole::Admin, Some("Ann")),
    ]);
    assert!(src.request_group(AccountId(1), ConversationId(40)));
    assert!(answer(&mut src, &sent, Some(d.clone())).group);
    assert!(src.take_group().is_some());

    // A message in the group asked again: the same page.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(40)],
    });
    let ch = answer(&mut src, &sent, Some(d.clone()));
    assert!(!ch.group && src.take_group().is_none(), "nothing to tell");

    // A real change goes through (and an unseen one is not lost behind an equal one).
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(40)],
    });
    let mut changed = d.clone();
    changed.description = Some("New".into());
    assert!(answer(&mut src, &sent, Some(changed.clone())).group);
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(40)],
    });
    assert!(!answer(&mut src, &sent, Some(changed)).group);
    assert!(
        matches!(src.take_group(), Some(GroupUpdate::Show(v)) if v.description == "New"),
        "the change not yet taken is still there"
    );

    // Closed and asked again: handed over again.
    src.close_group();
    assert!(src.request_group(AccountId(1), ConversationId(40)));
    assert!(answer(&mut src, &sent, Some(d)).group);
}

/// The page ends: its group is deleted, its account is removed or cannot be used, the answer is
/// "no such group"; closing stops the reading and drops a late answer.
#[test]
fn the_group_page_ends_with_its_group_its_account_and_an_empty_answer() {
    let store = Store::new(&[(1, 3)]);

    // The group is deleted; another chat's removal ends nothing.
    let (mut src, sent) = opened_source(&store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(77)],
    });
    assert!(!ch.group && src.take_group().is_none());
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(40)],
    });
    assert!(ch.group);
    assert_eq!(src.take_group(), Some(GroupUpdate::Ended));

    // The group is the OPEN chat (the usual way): the removal's early return tells of the end too.
    let (mut src, sent) = opened_source(&store);
    src.open_conversation(Some(ConversationId(40)));
    settle(&mut src, &sent, &store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(40)],
    });
    assert!(ch.group, "also when the chat itself closes");
    assert_eq!(src.take_group(), Some(GroupUpdate::Ended));

    // The account is removed.
    let (mut src, sent) = opened_source(&store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::AccountRemoved {
        account: AccountId(1),
    });
    assert!(ch.group);
    assert_eq!(src.take_group(), Some(GroupUpdate::Ended));
    // Another account's removal ends nothing.
    let (mut src, sent) = opened_source(&store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::AccountRemoved {
        account: AccountId(2),
    });
    assert!(!ch.group && src.take_group().is_none());

    // The account leaves the usable states.
    for state in [
        AccountState::LoggedOut {
            reason: String::new(),
        },
        AccountState::Unlinking,
        AccountState::Removing,
    ] {
        let (mut src, sent) = opened_source(&store);
        shown(&mut src, &sent);
        let mut gone = super::tests::account();
        gone.state = state;
        src.on_event(&UiEvent::AccountsChanged);
        let (token, _) = take_queries(&sent)
            .into_iter()
            .find(|(_, q)| matches!(q, Query::Accounts))
            .expect("accounts asked");
        let ch = src.on_event(&result(token, QueryData::Accounts(vec![gone])));
        assert!(ch.group);
        assert_eq!(src.take_group(), Some(GroupUpdate::Ended));
    }

    // The core does not have the group (or it is no group): the answer is empty.
    let (mut src, sent) = opened_source(&store);
    assert!(src.request_group(AccountId(1), ConversationId(40)));
    let ch = answer(&mut src, &sent, None);
    assert!(ch.group);
    assert_eq!(src.take_group(), Some(GroupUpdate::Ended));

    // Closing the page from the front end stops the reading and drops a late answer.
    let (mut src, sent) = opened_source(&store);
    assert!(src.request_group(AccountId(1), ConversationId(40)));
    let (token, _) = take_queries(&sent)
        .into_iter()
        .find(|(_, q)| matches!(q, Query::Group(_)))
        .expect("query");
    src.close_group();
    let ch = src.on_event(&result(
        token,
        QueryData::Group(Some(Box::new(detail(Vec::new())))),
    ));
    assert!(!ch.group && src.take_group().is_none());
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    assert_eq!(group_queries(&sent), 0, "nothing is shown, nothing is read");
}

/// Both pages of one panel (a member's page on top of the group's) end with the group they were
/// opened from, and each reads its own subject: the person's page does not read the group.
#[test]
fn a_members_page_on_top_of_the_group_ends_with_the_group() {
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened_source(&store);
    shown(&mut src, &sent);
    assert!(src.request_person(AccountId(1), user(2), Some(ConversationId(40))));
    take_queries(&sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(40)],
    });
    assert!(ch.group && ch.person, "both end with the group");
    assert_eq!(src.take_group(), Some(GroupUpdate::Ended));
    assert_eq!(src.take_person(), Some(crate::source::PersonUpdate::Ended));
}

/// The chat header counts the people IN the group, as the page does: an invitation or a request
/// to join is no member. This is the header of the real app (`CoreSource`), not the fixture's.
#[test]
fn the_chat_header_counts_full_members_only() {
    let mut store = Store::new(&[(1, 3)]);
    let row = |n: u32, role| Member {
        user: user(n),
        role,
    };
    store.members = vec![
        row(1, MemberRole::Member),
        row(2, MemberRole::Admin),
        row(3, MemberRole::Pending),
        row(4, MemberRole::Requesting),
    ];
    let (mut src, sent) = opened_source(&store);
    src.open_conversation(Some(ConversationId(1)));
    settle(&mut src, &sent, &store);
    let header = src.chat(ConversationId(1)).expect("header");
    assert_eq!(header.subtitle, "2 members");
    assert!(header.has_details, "a group's header opens its page");
}

/// A channel or a distribution list has no page: its header is a plain title.
#[test]
fn only_a_one_to_one_chat_and_a_group_have_details() {
    use kourier_model::ConvKind;
    assert!(crate::conversations::kind_has_details(ConvKind::Direct));
    assert!(crate::conversations::kind_has_details(ConvKind::Group));
    assert!(!crate::conversations::kind_has_details(ConvKind::Channel));
    assert!(!crate::conversations::kind_has_details(ConvKind::Broadcast));
}

/// A 1:1 chat's page does not offer the timer of a chat the user deleted from the list, but
/// still names the chat ("Message" stays).
#[test]
fn a_deleted_chat_offers_no_timer_but_is_still_there() {
    use kourier_core::facade::{PersonChat, PersonDetail};
    let mut features = ConversationFeatures::default();
    features
        .disappearing
        .insert(kourier_model::DisappearKind::AfterSend, vec![3600]);
    let chat = |hidden| PersonChat {
        id: ConversationId(5),
        timer: None,
        features: features.clone(),
        blocked: false,
        hidden,
    };
    let who: UserId = user(7);
    let page = |hidden| {
        let d = PersonDetail {
            account: AccountId(1),
            user: who.clone(),
            is_self: false,
            contact: None,
            own_name: None,
            own_number: None,
            chat: Some(chat(hidden)),
            groups: Vec::new(),
            safety_numbers: true,
            safety_waits: false,
            link: kourier_core::facade::LinkState::Hidden,
            share: kourier_core::facade::ShareState::Hidden,
        };
        crate::person::person_view(&d, kourier_model::NetworkKind::Signal, &|_| String::new())
    };
    let listed = page(false);
    assert!(!listed.timers.is_empty());
    let deleted = page(true);
    assert!(deleted.timers.is_empty() && deleted.timer_range.is_none());
    assert_eq!(deleted.chat, Some(ConversationId(5)), "Message stays");
}

/// P2b: a refresh that changes only a member row puts nothing on screen but the rows; any other
/// change of the page (timer, description, the number of members) puts the rest of it.
#[test]
fn only_a_change_of_the_rest_of_the_page_is_put_on_screen() {
    use MemberRole::Member as M;
    let base = detail(vec![
        mine(M),
        member(2, M, Some("Ann")),
        member(3, M, Some("Bob")),
    ]);
    let (rows, shown) = split_page(view(&base), None);
    let shown = shown.expect("the first page is shown");
    assert_eq!(
        (rows.len(), shown.members.len(), shown.member_count),
        (3, 0, 3)
    );

    // The same page again, and one member renamed: only rows.
    let (_, again) = split_page(view(&base), Some(&shown));
    assert!(again.is_none());
    let mut renamed = base.clone();
    renamed.members[1].name = Some("Anna".into());
    let (rows, none) = split_page(view(&renamed), Some(&shown));
    assert!(none.is_none());
    assert_eq!(rows.len(), 3);

    // The timer label, the description, one member more or less: the rest is shown.
    let mut timer = base.clone();
    timer.timer = Some(std::time::Duration::from_secs(3600));
    assert!(split_page(view(&timer), Some(&shown)).1.is_some());
    let mut text = base.clone();
    text.description = Some("About".into());
    assert!(split_page(view(&text), Some(&shown)).1.is_some());
    let mut joined = base.clone();
    joined.members.push(member(4, M, Some("Cy")));
    let (rows, rest) = split_page(view(&joined), Some(&shown));
    assert_eq!(rows.len(), 4);
    assert_eq!(rest.expect("a join changes the page").member_count, 4);
    let mut left = base;
    left.members.pop();
    assert_eq!(
        split_page(view(&left), Some(&shown))
            .1
            .expect("a leave")
            .member_count,
        2
    );
}
