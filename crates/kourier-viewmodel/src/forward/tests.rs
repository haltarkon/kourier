use kourier_core::facade::{ForwardPlan, PlannedForward, PlannedTarget};
use kourier_model::{AttachmentId, CannotSend, NetworkKind};

use super::*;

fn target(id: i64, account: i64, title: &str) -> ForwardTarget {
    ForwardTarget {
        id: ConversationId(id),
        account: AccountId(account),
        title: title.to_owned(),
        avatar_url: String::new(),
        is_self: false,
        can_send: true,
        archived: false,
        pinned_order: None,
        last_activity: id * 10,
        account_tag: format!("tag{account}"),
        tint: "#000000".into(),
        native: false,
    }
}

fn ids(rows: &[ForwardTarget]) -> Vec<i64> {
    rows.iter().map(|t| t.id.0).collect()
}

#[test]
fn targets_come_from_shown_accounts_that_can_send_archived_last() {
    let mut a = target(1, 1, "Ann");
    let b = target(2, 1, "Bob");
    let mut c = target(3, 2, "Cy");
    let d = target(4, 3, "Hidden account");
    let mut e = target(5, 1, "Closed");
    e.can_send = false;
    a.archived = true;
    c.is_self = true;
    let all = [a, b, c, d, e];
    let out = listed_forward_targets(&all, &[AccountId(1), AccountId(2)], AccountId(1), "");
    // Inbox by activity (newest first), then the archive; account 3 and the closed chat gone.
    assert_eq!(ids(&out), vec![3, 2, 1]);
    assert_eq!(
        out.iter().map(|t| t.native).collect::<Vec<_>>(),
        vec![false, true, true]
    );
    assert!(out.iter().all(|t| !t.account_tag.is_empty()));
}

#[test]
fn the_account_tag_stays_only_where_several_accounts_are_shown() {
    let all = [target(1, 1, "Ann"), target(2, 1, "Bob")];
    let out = listed_forward_targets(&all, &[AccountId(1)], AccountId(1), "");
    assert!(out.iter().all(|t| t.account_tag.is_empty() && t.native));
}

#[test]
fn the_search_matches_titles_without_case() {
    let all = [target(1, 1, "Ann Arden"), target(2, 1, "Bob")];
    let out = listed_forward_targets(&all, &[AccountId(1)], AccountId(1), "  ARDEN ");
    assert_eq!(ids(&out), vec![1]);
}

fn item(id: i64, sender: RunSender, minutes: i64) -> RunItem {
    RunItem {
        id: MessageId(id),
        sender,
        at: minutes * 60_000,
    }
}

fn user(n: &str) -> RunSender {
    RunSender::User(UserId::from_parts("e164", n).unwrap())
}

#[test]
fn runs_split_on_another_sender_or_an_hour() {
    let own = RunSender::Own;
    let items = [
        item(1, own.clone(), 0),
        item(2, own.clone(), 59),      // 59 min: same run
        item(3, own.clone(), 59 + 60), // 60 min: new run
        item(4, user("+491"), 119),
        item(5, user("+491"), 120),
        item(6, own.clone(), 121),  // another sender
        item(7, user("+491"), 122), // interleaved: its own run again
        item(8, user("+492"), 123),
    ];
    let firsts: Vec<i64> = run_firsts(&items).iter().map(|m| m.0).collect();
    assert_eq!(firsts, vec![1, 3, 4, 6, 7, 8]);
    assert!(run_firsts(&[]).is_empty());
}

#[test]
fn the_header_line_names_the_sender_and_the_time() {
    assert_eq!(
        header_line("Ann", "12 Sep 14:05"),
        "Forwarded from Ann · 12 Sep 14:05"
    );
}

fn planned(id: i64, v: ForwardVerdict) -> PlannedForward {
    PlannedForward {
        id: MessageId(id),
        row: None,
        verdict: v,
    }
}

fn ptarget(conv: i64, account: i64, native: bool) -> PlannedTarget {
    PlannedTarget {
        conv: ConversationId(conv),
        account: Some(AccountId(account)),
        native,
        refused: None,
        changes: Vec::new(),
    }
}

fn plan(messages: Vec<PlannedForward>, targets: Vec<PlannedTarget>) -> ForwardPlan {
    ForwardPlan {
        source: ConversationId(1),
        now: 0,
        messages,
        targets,
        to_download: Vec::new(),
        lost: Vec::new(),
        limits: Vec::new(),
        many: false,
    }
}

fn view(p: &ForwardPlan) -> ForwardDialogView {
    let title = |c: ConversationId| format!("chat{}", c.0);
    let network = |a: AccountId| {
        if a.0 == 1 {
            "WhatsApp".to_owned()
        } else {
            "Signal".to_owned()
        }
    };
    ForwardDialogView::new(
        p,
        &ViewFacts {
            title: &title,
            network: &network,
        },
    )
}

#[test]
fn the_title_counts_the_going_messages() {
    let one = plan(
        vec![planned(1, ForwardVerdict::Goes)],
        vec![ptarget(2, 1, true)],
    );
    assert_eq!(view(&one).title, "Forward Message");
    let two = plan(
        vec![
            planned(1, ForwardVerdict::Goes),
            planned(2, ForwardVerdict::Goes),
            planned(3, ForwardVerdict::ViewOnce),
        ],
        vec![ptarget(2, 1, true)],
    );
    let v = view(&two);
    assert_eq!(v.title, "Forward 2 Messages");
    assert_eq!(v.count, 2);
}

#[test]
fn every_left_out_kind_has_its_line() {
    let kinds = [
        ForwardVerdict::ViewOnce,
        ForwardVerdict::Notice,
        ForwardVerdict::Deleted,
        ForwardVerdict::Gone,
        ForwardVerdict::Protected,
        ForwardVerdict::Incomplete,
    ];
    let mut messages = vec![planned(100, ForwardVerdict::Goes)];
    for (i, k) in kinds.iter().enumerate() {
        messages.push(planned(i as i64, *k));
        messages.push(planned(50 + i as i64, *k));
    }
    let v = view(&plan(messages, vec![ptarget(2, 1, true)]));
    assert_eq!(v.left_out.len(), 6);
    assert!(
        v.left_out.iter().all(|l| l.starts_with("2 ")),
        "{:?}",
        v.left_out
    );
    // `Goes` has no line.
    assert!(left_out_line(ForwardVerdict::Goes, 1).is_none());
}

#[test]
fn a_refusal_has_words_for_every_variant() {
    // No `_` arm: a new variant fails to build here.
    let all = [
        ForwardRefusal::UnknownChat,
        ForwardRefusal::AccountNotLinked,
        ForwardRefusal::CannotSend(CannotSend::AdminsOnly),
        ForwardRefusal::NetworkOff,
        ForwardRefusal::TooManyChats,
        ForwardRefusal::NothingToSend,
        ForwardRefusal::SourceChanged,
        ForwardRefusal::FileNotHere,
        ForwardRefusal::Failed,
    ];
    for r in &all {
        match r {
            ForwardRefusal::UnknownChat
            | ForwardRefusal::AccountNotLinked
            | ForwardRefusal::CannotSend(_)
            | ForwardRefusal::NetworkOff
            | ForwardRefusal::TooManyChats
            | ForwardRefusal::NothingToSend
            | ForwardRefusal::SourceChanged
            | ForwardRefusal::FileNotHere
            | ForwardRefusal::Failed => {}
        }
        assert!(!refusal_text(r).is_empty());
    }
    assert_eq!(
        refusal_text(&ForwardRefusal::CannotSend(CannotSend::AdminsOnly)),
        "Only admins can send messages."
    );
}

#[test]
fn every_change_is_worded() {
    use ForwardChange as C;
    let all = [
        C::FormattingDropped(2),
        C::MentionsAsText(1),
        C::LinksAsText(3),
        C::FileAsGeneric(vec![AttachmentId(1)]),
        C::FileTooLarge(vec![AttachmentId(1), AttachmentId(2)]),
        C::FileRefused(vec![AttachmentId(1)]),
        C::StickerAsPicture(1),
        C::ContactAsText(1),
        C::LocationAsText(1),
        C::PollAsText(1),
        C::ReplyNotCarried(1),
    ];
    for c in &all {
        assert!(change_text(c).contains(&change_count(c).to_string()));
    }
}

#[test]
fn a_change_for_some_targets_names_them() {
    let mut a = ptarget(2, 2, false);
    a.changes = vec![
        ForwardChange::MentionsAsText(2),
        ForwardChange::LinksAsText(1),
    ];
    let mut b = ptarget(3, 2, false);
    b.changes = vec![ForwardChange::MentionsAsText(2)];
    let mut closed = ptarget(4, 2, false);
    closed.refused = Some(ForwardRefusal::CannotSend(CannotSend::Blocked));
    closed.changes = vec![ForwardChange::PollAsText(9)];
    let v = view(&plan(
        vec![planned(1, ForwardVerdict::Goes)],
        vec![a, b, closed],
    ));
    assert_eq!(
        v.changes,
        vec![
            "2 mentions become plain text.".to_owned(),
            "1 link becomes text with its address in chat2.".to_owned()
        ]
    );
    assert!(v.look_shown);
    assert!(v.can_forward);
    // The refusal words per target.
    assert_eq!(v.targets[0].1, "");
    assert_eq!(v.targets[2].1, "You blocked this contact.");
}

#[test]
fn a_native_only_forward_shows_no_look_and_a_refused_one_cannot_go() {
    let p = plan(
        vec![planned(1, ForwardVerdict::Goes)],
        vec![ptarget(2, 1, true)],
    );
    let v = view(&p);
    assert!(!v.look_shown);
    assert!(v.can_forward);
    let mut refused = ptarget(2, 1, true);
    refused.refused = Some(ForwardRefusal::AccountNotLinked);
    let v = view(&plan(vec![planned(1, ForwardVerdict::Goes)], vec![refused]));
    assert!(!v.can_forward);
    // No message goes.
    let v = view(&plan(
        vec![planned(1, ForwardVerdict::ViewOnce)],
        vec![ptarget(2, 1, true)],
    ));
    assert!(!v.can_forward);
}

#[test]
fn a_passed_limit_says_whose_and_blocks_forwarding() {
    let rules = ForwardRules {
        max_chats: Some(2),
        many_from: 5,
        many_max_chats: 1,
    };
    let mut targets: Vec<PlannedTarget> = (2..=4).map(|c| ptarget(c, 1, false)).collect();
    targets[2].refused = Some(ForwardRefusal::TooManyChats);
    let mut p = plan(vec![planned(1, ForwardVerdict::Goes)], targets);
    p.limits = vec![(AccountId(1), rules)];
    let v = view(&p);
    assert_eq!(
        v.limits,
        vec!["WhatsApp forwards to at most 2 chats at once."]
    );
    assert!(!v.can_forward);

    // The "many" rule: two native targets, the second held back.
    let mut targets = vec![ptarget(2, 1, true), ptarget(3, 1, true)];
    targets[1].refused = Some(ForwardRefusal::TooManyChats);
    let mut p = plan(vec![planned(1, ForwardVerdict::Goes)], targets);
    p.limits = vec![(AccountId(1), rules)];
    p.many = true;
    let v = view(&p);
    assert_eq!(
        v.limits,
        vec!["WhatsApp sends a message forwarded many times to one chat at a time."]
    );
}

#[test]
fn the_json_carries_the_counts_and_no_title_of_the_source() {
    let mut p = plan(
        vec![planned(1, ForwardVerdict::Goes)],
        vec![ptarget(2, 1, true)],
    );
    p.to_download = vec![AttachmentId(1), AttachmentId(2)];
    p.lost = vec![AttachmentId(3)];
    let j: serde_json::Value = serde_json::from_str(&view(&p).to_json()).unwrap();
    assert_eq!(j["downloads"], 2);
    assert_eq!(j["lost"], 1);
    assert_eq!(j["canForward"], true);
    assert_eq!(j["targets"][0]["id"], 2);
}

#[test]
fn the_end_notice_names_what_went_and_what_did_not() {
    let title = |c: ConversationId| format!("chat{}", c.0);
    let one = ended_notice(&[ConversationId(2)], &[], &title);
    assert_eq!(one, "Forwarded to chat2");
    let many = ended_notice(&[ConversationId(2), ConversationId(3)], &[], &title);
    assert_eq!(many, "Forwarded to 2 chats");
    let mixed = ended_notice(
        &[ConversationId(2)],
        &[(ConversationId(3), ForwardRefusal::TooManyChats)],
        &title,
    );
    assert_eq!(
        mixed,
        "Forwarded to chat2\nNot forwarded to chat3: Too many chats for one forward."
    );
}

#[test]
fn debug_prints_no_text() {
    let u = ForwardUpdate::Ended {
        request: 3,
        notice: "Forwarded to Secret".into(),
    };
    assert!(!format!("{u:?}").contains("Secret"));
    let d = ForwardUpdate::Dialog("{\"title\":\"Secret\"}".into());
    assert!(!format!("{d:?}").contains("Secret"));
    let t = target(1, 1, "Secret");
    assert!(!format!("{t:?}").contains("Secret"));
    let _ = NetworkKind::Fake;
}
