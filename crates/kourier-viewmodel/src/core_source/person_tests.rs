//! `CoreSource` and `person_view`: the person's page of the details panel (people pages P1),
//! against the simulated core of [`super::tests`].

use kourier_core::facade::{CommonGroup, PersonChat, PersonDetail, Query, QueryData};
use kourier_model::{
    AccountId, AccountState, Contact, ContactId, ConversationFeatures, ConversationId, Identifier,
    IdentifierKind, NetworkKind, TrustLevel, UserId,
};

use super::tests::{Sent, Store, result, settle, source, take_queries, user};
use super::*;
use crate::person::{GROUPS_PREVIEW, PersonView, person_view};
use crate::source::PersonUpdate;

fn contact(user: &UserId) -> Contact {
    Contact {
        id: ContactId(1),
        account: AccountId(1),
        user: user.clone(),
        profile_name: None,
        system_name: None,
        nickname: None,
        avatar: None,
        trust: TrustLevel::Unverified,
        identity_fingerprint: None,
        blocked: false,
        hidden: false,
        unregistered_at: None,
        identifiers: Vec::new(),
        meta: None,
        about: None,
        about_emoji: None,
        number_shown: None,
    }
}

fn detail(user: &UserId) -> PersonDetail {
    PersonDetail {
        account: AccountId(1),
        user: user.clone(),
        is_self: false,
        contact: Some(contact(user)),
        own_name: None,
        own_number: None,
        chat: None,
        groups: Vec::new(),
        safety_numbers: true,
        safety_waits: false,
        link: kourier_core::facade::LinkState::Hidden,
        share: kourier_core::facade::ShareState::Hidden,
    }
}

fn phone(number: &str) -> Identifier {
    Identifier {
        kind: IdentifierKind::Phone,
        value: number.to_owned(),
    }
}

fn view(d: &PersonDetail) -> PersonView {
    person_view(d, NetworkKind::Signal, &|_| String::new())
}

/// The name on top follows the core's rule (nickname, address book, profile name, else a label
/// made from the id), and the profile name is added only when it differs.
#[test]
fn the_name_on_top_follows_the_nickname_the_address_book_and_the_profile() {
    let who = user(7);
    let mut d = detail(&who);
    let c = d.contact.as_mut().expect("contact");
    c.profile_name = Some("Alice Profile".into());
    assert_eq!(view(&d).name, "Alice Profile");
    assert_eq!(view(&d).profile_name, "", "the same name is not repeated");
    d.contact.as_mut().expect("contact").system_name = Some("Alice Book".into());
    let v = view(&d);
    assert_eq!(
        (v.name.as_str(), v.profile_name.as_str()),
        ("Alice Book", "Alice Profile")
    );
    d.contact.as_mut().expect("contact").nickname = Some("Ally".into());
    assert_eq!(view(&d).name, "Ally");
    // No contact at all: a label from the id, never the id.
    d.contact = None;
    let v = view(&d);
    assert!(v.name.contains("Signal user"), "{}", v.name);
    assert!(!v.name.contains("00000000-0000"), "{}", v.name);
    // The user's own page: the account's own name.
    d.is_self = true;
    d.own_name = Some("My Name".into());
    assert_eq!(view(&d).name, "My Name");
}

/// The about text is shown with its emoji in front, as one escaped text; its links stand only where
/// her 1:1 chat exists and is not a request, an invitation or blocked (critic M2: a stranger's text is
/// plain, and no chat at all is a stranger). One row for every state of the chat; the text is the same
/// whether it is linked or not.
#[test]
fn the_about_text_links_only_where_her_chat_is_accepted() {
    use kourier_model::CannotSend;
    let who = user(7);
    let text = "Find me at https://example.org/me or example.net";
    // `blocks`: the chat's `blocked` and the contact's `blocked` (H1).
    let with_blocks = |chat: Option<Option<CannotSend>>, blocks: (bool, bool)| {
        let mut d = detail(&who);
        let c = d.contact.as_mut().expect("contact");
        c.about = Some(text.into());
        c.about_emoji = Some("🙂".into());
        c.blocked = blocks.1;
        d.chat = chat.map(|cannot_send| PersonChat {
            id: ConversationId(5),
            timer: None,
            features: ConversationFeatures {
                cannot_send,
                ..ConversationFeatures::default()
            },
            blocked: blocks.0,
            hidden: false,
        });
        view(&d)
    };
    let with = |chat: Option<Option<CannotSend>>| with_blocks(chat, (false, false));
    let rows = [
        // No chat at all: a stranger.
        (None, false),
        (Some(None), true),
        (Some(Some(CannotSend::NotYetBuilt)), true),
        (Some(Some(CannotSend::AdminsOnly)), true),
        (Some(Some(CannotSend::Unregistered)), true),
        (Some(Some(CannotSend::Other("another reason".into()))), true),
        (Some(Some(CannotSend::RequestPending)), false),
        (Some(Some(CannotSend::NotAccepted)), false),
        (Some(Some(CannotSend::InvitationPending)), false),
        (Some(Some(CannotSend::JoinRequestPending)), false),
        (Some(Some(CannotSend::Blocked)), false),
    ];
    for (chat, linked) in rows {
        let v = with(chat.clone());
        assert_eq!(v.about_links, linked, "chat {chat:?}");
        assert_eq!(
            v.about_html.contains("<a href=\"https://example.org/me\">"),
            linked,
            "chat {chat:?}: {}",
            v.about_html
        );
        assert_eq!(
            v.about_html.contains("<a href=\"https://example.net\">"),
            linked
        );
        let want: &[&str] = if linked {
            &["https://example.org/me", "https://example.net"]
        } else {
            &[]
        };
        assert_eq!(v.about_hrefs, want, "chat {chat:?}");
        // The emoji and a space come first, whether linked or not; the plain fields are as they were.
        assert!(
            v.about_html.starts_with("🙂 Find me at "),
            "{}",
            v.about_html
        );
        assert_eq!((v.about.as_str(), v.about_emoji.as_str()), (text, "🙂"));
    }
    // H1: no backend states `CannotSend::Blocked`; the block is the chat's own fact (`conversations.blocked`) or the
    // contact's (`Contact.blocked`), and an accepted chat (`cannot_send` None) with either has no link.
    for blocks in [(true, false), (false, true), (true, true)] {
        let v = with_blocks(Some(None), blocks);
        assert!(
            !v.about_links && v.about_hrefs.is_empty() && !v.about_html.contains("<a "),
            "blocks (chat, contact) {blocks:?}: {}",
            v.about_html
        );
        assert_eq!(v.about, text, "the text is the same: blocks {blocks:?}");
    }
}

/// An about text with markup is shown as text; an emoji alone is a text of its own; nothing is none.
#[test]
fn the_about_html_escapes_markup_and_needs_a_text_or_an_emoji() {
    let who = user(7);
    let mut d = detail(&who);
    assert_eq!(view(&d).about_html, "");
    d.contact.as_mut().expect("contact").about = Some("<img src=x> & \"q\"".into());
    assert_eq!(view(&d).about_html, "&lt;img src=x&gt; &amp; &quot;q&quot;");
    let c = d.contact.as_mut().expect("contact");
    c.about = None;
    c.about_emoji = Some("🙂".into());
    let v = view(&d);
    assert_eq!(v.about_html, "🙂");
    assert!(!v.about_links, "no chat: a stranger");
}

/// WhatsApp puts a "~" in front of the name a person chose for themself (his answer "~Name, as
/// the phone", W6a2 B8c): on the top line when no address-book name or nickname comes first, and
/// on the profile line when one does. The name is never marked twice and no other network gets
/// the mark.
#[test]
fn a_whatsapp_chosen_name_carries_the_mark_on_the_page() {
    let who = user(7);
    let mut d = detail(&who);
    let wa = |d: &PersonDetail| person_view(d, NetworkKind::WhatsApp, &|_| String::new());
    d.contact.as_mut().expect("contact").profile_name = Some("Alice Chosen".into());
    let v = wa(&d);
    assert_eq!(
        (v.name.as_str(), v.profile_name.as_str()),
        ("~Alice Chosen", ""),
        "the chosen name on top, marked, and not repeated below"
    );
    d.contact.as_mut().expect("contact").system_name = Some("Alice Book".into());
    let v = wa(&d);
    assert_eq!(
        (v.name.as_str(), v.profile_name.as_str()),
        ("Alice Book", "~Alice Chosen"),
        "the address book wins unmarked; the chosen name below keeps its mark"
    );
    d.contact.as_mut().expect("contact").nickname = Some("Ally".into());
    assert_eq!(wa(&d).name, "Ally", "a nickname wins unmarked");
    // The same person on Signal: no mark anywhere.
    let v = view(&d);
    assert_eq!(
        (v.name.as_str(), v.profile_name.as_str()),
        ("Ally", "Alice Chosen")
    );
    // No profile name at all: no lone mark.
    d.contact.as_mut().expect("contact").profile_name = None;
    d.contact.as_mut().expect("contact").nickname = None;
    d.contact.as_mut().expect("contact").system_name = None;
    assert!(!wa(&d).name.contains('~'), "{}", wa(&d).name);
}

/// A number is on the page only when the backend said it may be shown (`Some(true)`); "never
/// said" and "no" show no line at all. A person known by number alone shows it; the user's own
/// page shows the account's number.
#[test]
fn the_number_shows_only_when_the_backend_said_so() {
    let who = user(7);
    let mut d = detail(&who);
    {
        let c = d.contact.as_mut().expect("contact");
        c.identifiers = vec![phone("+4915112345678")];
    }
    for (shown, line) in [
        (None, ""),
        (Some(false), ""),
        (Some(true), "+4915112345678"),
    ] {
        d.contact.as_mut().expect("contact").number_shown = shown;
        assert_eq!(view(&d).number, line, "number_shown {shown:?}");
    }
    // Shown but the core has no number for them: nothing to show.
    d.contact.as_mut().expect("contact").identifiers.clear();
    assert_eq!(view(&d).number, "");

    let by_number = UserId::parse("e164:+4915100000001").expect("id");
    let mut n = detail(&by_number);
    n.contact = None;
    // The number names them, so it is the name and not a second line with the same text.
    let named = view(&n);
    assert_eq!(named.name, "+4915100000001", "nothing else names them");
    assert_eq!(named.number, "", "the number is not shown twice");
    // With a name of their own the number is a line of its own.
    let mut n = detail(&by_number);
    n.contact.as_mut().expect("contact").nickname = Some("Nick".into());
    let nick = view(&n);
    assert_eq!(
        (nick.name.as_str(), nick.number.as_str()),
        ("Nick", "+4915100000001")
    );

    let mut me = detail(&who);
    me.is_self = true;
    me.own_number = Some("+15550100".into());
    assert_eq!(view(&me).number, "+15550100");
}

/// "Blocked" and "Not on Signal any more" are short lines under the name.
#[test]
fn blocked_and_gone_people_say_so() {
    let mut d = detail(&user(7));
    assert!(view(&d).notes.is_empty());
    {
        let c = d.contact.as_mut().expect("contact");
        c.blocked = true;
        c.unregistered_at = Some(5);
    }
    assert_eq!(
        view(&d).notes,
        vec!["Blocked".to_owned(), "Not on Signal any more".to_owned()]
    );
}

fn group(id: i64, title: &str) -> CommonGroup {
    CommonGroup {
        id: ConversationId(id),
        title: Some(title.into()),
        avatar: None,
    }
}

/// The groups in common come by name, and "Show All (N)" appears only past five.
#[test]
fn the_groups_come_by_name_and_are_cut_at_five_with_show_all() {
    let mut d = detail(&user(7));
    d.groups = vec![group(3, "zebra"), group(1, "Alpha"), group(2, "beta")];
    let v = view(&d);
    let titles: Vec<&str> = v.groups.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(titles, ["Alpha", "beta", "zebra"]);
    assert_eq!(v.groups_more, "", "three groups need no Show All");
    d.groups = (1..=7).map(|i| group(i, &format!("G{i}"))).collect();
    let v = view(&d);
    assert_eq!(v.groups.len(), 7);
    assert_eq!(GROUPS_PREVIEW, 5);
    assert_eq!(v.groups_more, "Show All (7)");
    assert_eq!(v.groups_preview, 5);
    d.groups.truncate(GROUPS_PREVIEW);
    assert_eq!(view(&d).groups_more, "");
    d.groups.push(CommonGroup {
        id: ConversationId(99),
        title: None,
        avatar: None,
    });
    assert!(view(&d).groups.iter().any(|g| g.title == "Unnamed group"));
}

/// The cut follows Signal Desktop's rule, the same as the members of a group: "Show All" appears
/// only when it would hide TWO or more, so a list of 6 shows all 6 and a list of 7 shows 5.
#[test]
fn show_all_hides_two_or_more_groups_never_one() {
    let mut d = detail(&user(7));
    for (total, preview, more) in [
        (5, 5, ""),
        (6, 6, "" /* one more would be hidden: shown instead */),
        (7, 5, "Show All (7)"),
        (8, 5, "Show All (8)"),
    ] {
        d.groups = (1..=total).map(|i| group(i, &format!("G{i}"))).collect();
        let v = view(&d);
        assert_eq!(
            (v.groups_preview, v.groups_more.as_str()),
            (preview, more),
            "{total} groups"
        );
        assert_eq!(v.groups.len(), total as usize, "the list itself is whole");
    }
}

/// The timer is the 1:1 chat's own: the options come from its features, and a chat we cannot
/// send to offers none.
#[test]
fn the_timer_belongs_to_the_persons_chat() {
    let mut d = detail(&user(7));
    assert!(view(&d).chat.is_none() && view(&d).timers.is_empty());
    d.chat = Some(PersonChat {
        id: ConversationId(5),
        timer: Some(std::time::Duration::from_secs(3600)),
        features: ConversationFeatures::default(),
        blocked: false,
        hidden: false,
    });
    let v = view(&d);
    assert_eq!(v.chat, Some(ConversationId(5)));
    assert_eq!(v.timer_secs, 3600);
    assert!(!v.timer.is_empty());

    // The times the chat offers: a page that can change them, and none for a chat that cannot (L14): a blocked one
    // (the header offers no timer for it either), a deleted one, one we cannot send to.
    d.chat.as_mut().expect("chat").features.disappearing = std::collections::BTreeMap::from([(
        kourier_model::DisappearKind::AfterSend,
        vec![30, 3600],
    )]);
    assert!(
        !view(&d).timers.is_empty(),
        "an accepted chat offers its times"
    );
    for what in ["blocked", "deleted", "cannot send"] {
        let mut d = d.clone();
        let chat = d.chat.as_mut().expect("chat");
        match what {
            "blocked" => chat.blocked = true,
            "deleted" => chat.hidden = true,
            _ => chat.features.cannot_send = Some(kourier_model::CannotSend::NotAccepted),
        }
        let v = view(&d);
        assert!(v.timers.is_empty(), "a {what} chat offers no timer");
        assert_eq!(v.timer_secs, 3600, "a {what} chat still says its time");
    }
}

/// Nothing personal reaches `Debug`.
#[test]
fn debug_of_the_view_shows_no_name_text_or_number() {
    let who = user(7);
    let mut d = detail(&who);
    {
        let c = d.contact.as_mut().expect("contact");
        c.profile_name = Some("Secret Name".into());
        c.about = Some("Secret about".into());
        c.number_shown = Some(true);
        c.identifiers = vec![phone("+4915199999999")];
    }
    d.groups = vec![group(1, "Secret Group")];
    let v = view(&d);
    let shown = format!("{v:?} {d:?} {:?}", PersonUpdate::Show(Box::new(v.clone())));
    for leak in ["Secret", "4915199999999"] {
        assert!(!shown.contains(leak), "{leak} in {shown}");
    }
}

/// P5a: "View Safety Number" is offered for another person of a network with safety numbers,
/// never on the own page, for a person known by a number alone, or on a network without them
/// (critic F5); its quiet line follows the contact's trust.
#[test]
fn safety_row_is_shown_for_another_person_of_a_network_with_numbers() {
    let who = user(7);
    let d = detail(&who);
    let v = view(&d);
    assert!(v.safety_shown);
    assert_eq!(v.safety_state, "", "not verified: no line");
    let mut own = d.clone();
    own.is_self = true;
    assert!(!view(&own).safety_shown, "never the own page");
    let mut by_number = d.clone();
    by_number.user = UserId::parse("e164:+4915100000001").expect("id");
    assert!(
        !view(&by_number).safety_shown,
        "a number alone has no account id"
    );
    let mut other_network = d.clone();
    other_network.safety_numbers = false;
    assert!(
        !view(&other_network).safety_shown,
        "a network without numbers"
    );
}

/// The quiet line under the row: "Verified" and "Safety number changed" from the contact's
/// trust; nothing for an unverified person or a hidden row.
#[test]
fn safety_row_state_line_follows_the_trust() {
    let who = user(7);
    let mut d = detail(&who);
    d.contact.as_mut().expect("contact").trust = TrustLevel::Verified;
    assert_eq!(view(&d).safety_state, "Verified");
    d.contact.as_mut().expect("contact").trust = TrustLevel::Changed;
    assert_eq!(view(&d).safety_state, "Safety number changed");
    d.safety_numbers = false;
    assert_eq!(view(&d).safety_state, "", "no row, no line");
}

/// U7: a person who was never verified, whose key changed and whose sends wait for the new
/// number (`safety_waits`, the fact the level gets as `waits`), shows "Safety number changed"
/// under the row; without a wait, or without the row, nothing; a verified person keeps
/// "Verified" until the contact's trust says otherwise.
#[test]
fn safety_row_says_changed_while_the_sends_wait() {
    let who = user(7);
    let mut d = detail(&who);
    assert_eq!(view(&d).safety_state, "", "never verified, nothing waits");
    d.safety_waits = true;
    assert_eq!(view(&d).safety_state, "Safety number changed");
    d.contact.as_mut().expect("contact").trust = TrustLevel::Verified;
    assert_eq!(view(&d).safety_state, "Verified");
    d.contact = None;
    assert_eq!(
        view(&d).safety_state,
        "Safety number changed",
        "no contact row: the wait alone says it"
    );
    d.safety_numbers = false;
    assert_eq!(view(&d).safety_state, "", "no row, no line");
}

/// P5b: "Copy Link" follows the core's `LinkState`: hidden (no links, the own page), the link
/// held with no line, or disabled with the sentence that says why (the person's name in it).
/// The link never shows in `Debug`.
#[test]
fn link_button_follows_the_link_state() {
    use kourier_core::facade::LinkState;
    let who = user(7);
    let mut d = detail(&who);
    d.contact.as_mut().expect("contact").profile_name = Some("Ada".into());
    let v = view(&d);
    assert!(
        !v.link_shown && v.link_note.is_empty() && v.link.is_none(),
        "hidden"
    );
    let url = "https://signal.me/#p/+15550001111";
    d.link = LinkState::Link(kourier_model::Redacted::new(url.into()));
    let v = view(&d);
    assert!(v.link_shown && v.link_note.is_empty());
    assert_eq!(v.link.as_ref().map(|l| l.expose().as_str()), Some(url));
    assert!(!format!("{v:?}").contains("555"), "Debug hides the link");
    d.link = LinkState::NotShared;
    let v = view(&d);
    assert!(v.link_shown && v.link.is_none());
    // Z1: neutral, true of a person who does not share it and of one never fetched.
    assert_eq!(v.link_note, "Ada's phone number is not shown.");
    assert!(
        !v.link_note.contains("share"),
        "no claim about what she shares"
    );
    d.link = LinkState::NoNumber;
    let v = view(&d);
    assert!(v.link_shown && v.link.is_none());
    assert_eq!(v.link_note, "Kourier does not know Ada's phone number.");
}

/// U7: the core's `SafetyWaitChanged` of the shown person asks the page again (another
/// person's does not), and the answer that says the sends wait shows the line.
#[test]
fn a_safety_wait_asks_the_shown_page_again() {
    let who = user(7);
    let (mut src, sent) = opened_person(&Store::new(&[(1, 3)]));
    assert!(src.request_person(AccountId(1), who.clone(), None));
    answer(&mut src, &sent, Some(detail(&who)));
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("a page");
    };
    assert_eq!(v.safety_state, "");
    let wait = |user| UiEvent::SafetyWaitChanged {
        account: AccountId(1),
        user,
    };
    src.on_event(&wait(user(8)));
    assert_eq!(
        person_queries(&sent),
        0,
        "another person's wait asks nothing"
    );
    src.on_event(&wait(who.clone()));
    let mut waiting = detail(&who);
    waiting.safety_waits = true;
    assert!(answer(&mut src, &sent, Some(waiting)).person);
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("a page");
    };
    assert_eq!(
        v.safety_state, "Safety number changed",
        "the shown person's wait asked again, and the line follows"
    );
}

/// F4 (critic M6): a change of her chat's timer that failed is said on HER page, under the selector, and
/// not only in the open chat's composer (hidden under a covering panel, or another chat's). Only the page of
/// the chat that failed says it, a refresh of the page carries it on, the next change of a timer forgets it
/// (the front end is told to read the page again), and a new ask for the page starts without it.
#[test]
fn a_failed_timer_change_is_said_on_her_page_until_the_next_change_or_page() {
    use kourier_core::facade::ActionSubject;
    let who = user(7);
    let her = ConversationId(5);
    let with_chat = |about: &str| {
        let mut d = detail(&who);
        d.contact.as_mut().expect("contact").about = Some(about.into());
        d.chat = Some(PersonChat {
            id: her,
            timer: None,
            features: ConversationFeatures::default(),
            blocked: false,
            hidden: false,
        });
        d
    };
    let network_no = || -> kourier_core::facade::CoreError {
        kourier_model::BackendError::new(kourier_model::ErrorKind::InvalidInput, "refused").into()
    };
    let core_no = || kourier_core::facade::CoreError::InvalidInput("reworded");
    let said = "The disappearing-message timer could not be changed.";
    let (mut src, sent) = opened_person(&Store::new(&[(1, 3)]));
    assert!(src.request_person(AccountId(1), who.clone(), None));
    answer(&mut src, &sent, Some(with_chat("first")));
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("a page");
    };
    assert_eq!(v.timer_failed, "", "nothing failed yet");

    // Another chat's failure is not hers: the composer's notice only.
    let other = ActionSubject::Conversation(ConversationId(9));
    let ch = src.on_action_failed("SetConversationTimer", &network_no(), &other);
    assert!(ch.notice && !ch.person && !ch.group);
    assert!(src.take_person().is_none());

    // Hers: the page says it, and the notice stays too.
    let subject = ActionSubject::Conversation(her);
    let ch = src.on_action_failed("SetConversationTimer", &network_no(), &subject);
    assert!(ch.person && ch.notice, "the page is told, and the composer");
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("her page, with the failure");
    };
    assert_eq!(v.timer_failed, said);
    assert_eq!(src.notice(), said, "the composer's notice is as it was");

    // A refresh of the page (her about text changed) carries the failure on.
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    assert!(answer(&mut src, &sent, Some(with_chat("second"))).person);
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("her page, refreshed");
    };
    assert_eq!(
        (v.about.as_str(), v.timer_failed.as_str()),
        ("second", said)
    );

    // The next change of a timer (any chat's) forgets it, and the page is read again by the front end.
    assert!(src.set_timer(ConversationId(9), 3600));
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("her page, without the failure");
    };
    assert_eq!(v.timer_failed, "");
    // (A change with no failure on show tells the page nothing.)
    assert!(src.set_timer(her, 3600));
    assert!(src.take_person().is_none());

    // The core's own refusal says the other sentence, on the page as in the composer.
    src.on_action_failed("SetConversationTimer", &core_no(), &subject);
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("her page");
    };
    assert_eq!(
        v.timer_failed,
        "This timer is not available in this conversation."
    );

    // A new ask for a page starts without it.
    assert!(src.request_person(AccountId(1), who.clone(), None));
    answer(&mut src, &sent, Some(with_chat("third")));
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("a page again");
    };
    assert_eq!(v.timer_failed, "", "the next page does not carry it");
}

fn opened_person(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    settle(&mut src, &sent, store);
    (src, sent)
}

/// The person queries sent so far (others, such as the list's, are taken out and ignored).
fn person_queries(sent: &Sent) -> usize {
    take_queries(sent)
        .iter()
        .filter(|(_, q)| matches!(q, Query::Person { .. }))
        .count()
}

fn commands_named(sent: &Sent, name: &str) -> usize {
    sent.borrow()
        .iter()
        .filter(|c| format!("{c:?}").starts_with(name))
        .count()
}

fn answer(src: &mut CoreSource, sent: &Sent, detail: Option<PersonDetail>) -> Changes {
    let asked = take_queries(sent);
    let (token, query) = asked
        .iter()
        .rev()
        .find(|(_, q)| matches!(q, Query::Person { .. }))
        .cloned()
        .expect("a person query");
    assert!(matches!(query, Query::Person { .. }), "{query:?}");
    src.on_event(&result(token, QueryData::Person(detail.map(Box::new))))
}

/// Opening a page asks for the person AND for a fresh profile, once; every change of a contact
/// reads the page again (coalesced: one query in flight); the answer names the canonical person.
#[test]
fn opening_a_page_asks_the_person_and_one_profile_refresh_and_changes_read_it_again() {
    let who = user(7);
    let (mut src, sent) = opened_person(&Store::new(&[(1, 3)]));
    assert!(src.request_person(AccountId(1), who.clone(), Some(ConversationId(5))));
    assert_eq!(
        commands_named(&sent, "RefreshContact"),
        1,
        "a fresh profile, asked once for this open"
    );
    let mut d = detail(&who);
    d.contact.as_mut().expect("contact").profile_name = Some("Alice".into());
    let ch = answer(&mut src, &sent, Some(d.clone()));
    assert!(ch.person);
    let Some(PersonUpdate::Show(v)) = src.take_person() else {
        panic!("a page");
    };
    assert_eq!(v.name, "Alice");
    assert!(src.take_person().is_none(), "taken once");

    // A contact changed: read again, even many times in a row, one query in flight.
    for _ in 0..3 {
        src.on_event(&UiEvent::ContactsChanged {
            account: AccountId(1),
        });
    }
    assert_eq!(person_queries(&sent), 1, "coalesced");
    assert_eq!(
        commands_named(&sent, "RefreshContact"),
        1,
        "the changes do not ask the network again"
    );
}

/// The page ends: its chat is deleted, its account is removed or cannot be used, the answer is
/// "no such account"; and a merged chat is followed.
#[test]
fn the_page_ends_with_its_chat_its_account_and_an_empty_answer() {
    let who = user(7);
    let shown = |src: &mut CoreSource, sent: &Sent| {
        assert!(src.request_person(AccountId(1), who.clone(), Some(ConversationId(5))));
        let mut d = detail(&who);
        d.chat = Some(PersonChat {
            id: ConversationId(5),
            timer: None,
            features: ConversationFeatures::default(),
            blocked: false,
            hidden: false,
        });
        answer(src, sent, Some(d));
        assert!(matches!(src.take_person(), Some(PersonUpdate::Show(_))));
    };
    let store = Store::new(&[(1, 3)]);

    // The chat it belongs to is deleted.
    let (mut src, sent) = opened_person(&store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(5)],
    });
    assert!(ch.person);
    assert_eq!(src.take_person(), Some(PersonUpdate::Ended));
    // An unrelated chat does not end it.
    let (mut src, sent) = opened_person(&store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(77)],
    });
    assert!(!ch.person && src.take_person().is_none());

    // A merge moves the chat; the removal of the survivor then ends it.
    let (mut src, sent) = opened_person(&store);
    shown(&mut src, &sent);
    src.on_event(&UiEvent::ConversationMerged {
        from: ConversationId(5),
        into: ConversationId(6),
    });
    take_queries(&sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(6)],
    });
    assert!(ch.person);
    assert_eq!(src.take_person(), Some(PersonUpdate::Ended));

    // The account is removed.
    let (mut src, sent) = opened_person(&store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::AccountRemoved {
        account: AccountId(1),
    });
    assert!(ch.person);
    assert_eq!(src.take_person(), Some(PersonUpdate::Ended));

    // The account leaves the usable states (logged out, unlinking).
    for state in [
        AccountState::LoggedOut {
            reason: String::new(),
        },
        AccountState::Unlinking,
        AccountState::Removing,
    ] {
        let (mut src, sent) = opened_person(&store);
        shown(&mut src, &sent);
        let mut gone = super::tests::account();
        gone.state = state;
        src.on_event(&UiEvent::AccountsChanged);
        let (token, _) = take_queries(&sent)
            .into_iter()
            .find(|(_, q)| matches!(q, Query::Accounts))
            .expect("accounts asked");
        let ch = src.on_event(&result(token, QueryData::Accounts(vec![gone])));
        assert!(ch.person);
        assert_eq!(src.take_person(), Some(PersonUpdate::Ended));
    }

    // The core does not know the account any more: the answer is empty.
    let (mut src, sent) = opened_person(&store);
    assert!(src.request_person(AccountId(1), who.clone(), Some(ConversationId(5))));
    let ch = answer(&mut src, &sent, None);
    assert!(ch.person);
    assert_eq!(src.take_person(), Some(PersonUpdate::Ended));

    // Closing the page from the front end stops the reading and drops a late answer.
    let (mut src, sent) = opened_person(&store);
    assert!(src.request_person(AccountId(1), who.clone(), Some(ConversationId(5))));
    let (token, _) = take_queries(&sent)
        .into_iter()
        .find(|(_, q)| matches!(q, Query::Person { .. }))
        .expect("query");
    src.close_person();
    let ch = src.on_event(&result(
        token,
        QueryData::Person(Some(Box::new(detail(&who)))),
    ));
    assert!(!ch.person && src.take_person().is_none());
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    assert_eq!(
        person_queries(&sent),
        0,
        "nothing is shown, nothing is read"
    );
}

/// A page opened from a GROUP's member list ends with that group, not with the person's own 1:1
/// chat: that chat may be one the user deleted (hidden), and its removal only changes what the
/// page offers (people pages P2, F9).
#[test]
fn a_page_opened_from_a_group_ends_with_the_group_not_with_the_persons_own_chat() {
    let who = user(7);
    let (group_chat, her_chat) = (ConversationId(40), ConversationId(5));
    let store = Store::new(&[(1, 3)]);
    let shown = |src: &mut CoreSource, sent: &Sent| {
        assert!(src.request_person(AccountId(1), who.clone(), Some(group_chat)));
        let mut d = detail(&who);
        d.chat = Some(PersonChat {
            id: her_chat,
            timer: None,
            features: ConversationFeatures::default(),
            blocked: false,
            hidden: true,
        });
        answer(src, sent, Some(d));
        assert!(matches!(src.take_person(), Some(PersonUpdate::Show(_))));
    };

    // Her own (hidden) chat is removed: the page stays and reads itself again.
    let (mut src, sent) = opened_person(&store);
    shown(&mut src, &sent);
    take_queries(&sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![her_chat],
    });
    assert!(!ch.person && src.take_person().is_none(), "it stays");
    assert_eq!(person_queries(&sent), 1, "and is read again");

    // The group it was opened from is removed: it ends.
    let (mut src, sent) = opened_person(&store);
    shown(&mut src, &sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![group_chat],
    });
    assert!(ch.person);
    assert_eq!(src.take_person(), Some(PersonUpdate::Ended));

    // A merge of the origin moves it; the survivor's removal ends the page.
    let (mut src, sent) = opened_person(&store);
    shown(&mut src, &sent);
    src.on_event(&UiEvent::ConversationMerged {
        from: group_chat,
        into: ConversationId(41),
    });
    take_queries(&sent);
    let ch = src.on_event(&UiEvent::ConversationsRemoved {
        ids: vec![ConversationId(41)],
    });
    assert!(ch.person);
}

/// An answer equal to the page on screen changes nothing (nothing is handed to the front end);
/// the first answer of a new ask is always handed over, also when it equals the page of the ask
/// before.
#[test]
fn an_answer_equal_to_the_page_on_screen_changes_nothing() {
    let who = user(7);
    let store = Store::new(&[(1, 3)]);
    let (mut src, sent) = opened_person(&store);
    assert!(src.request_person(AccountId(1), who.clone(), Some(ConversationId(5))));
    let mut d = detail(&who);
    d.contact.as_mut().expect("contact").profile_name = Some("Alice".into());
    assert!(answer(&mut src, &sent, Some(d.clone())).person);
    assert!(matches!(src.take_person(), Some(PersonUpdate::Show(_))));

    // Asked again by a message somewhere: the same page.
    src.on_event(&UiEvent::ConversationsChanged {
        ids: vec![ConversationId(9)],
    });
    let ch = answer(&mut src, &sent, Some(d.clone()));
    assert!(!ch.person && src.take_person().is_none(), "nothing to tell");

    // A real change goes through.
    src.on_event(&UiEvent::ContactsChanged {
        account: AccountId(1),
    });
    d.contact.as_mut().expect("contact").profile_name = Some("Alicia".into());
    assert!(answer(&mut src, &sent, Some(d.clone())).person);
    assert!(matches!(src.take_person(), Some(PersonUpdate::Show(_))));

    // Closed and asked again: the same page is handed over again (the front end cleared it).
    src.close_person();
    assert!(src.request_person(AccountId(1), who.clone(), Some(ConversationId(5))));
    assert!(answer(&mut src, &sent, Some(d)).person);
    assert!(matches!(src.take_person(), Some(PersonUpdate::Show(_))));
}

/// P5c: "Share Contact…" follows the core's `ShareState`: hidden (no cards, the own page), no line
/// when a card can be sent, else Copy Link's number sentences or SC1's "no name" line.
#[test]
fn share_button_follows_the_share_state() {
    use kourier_core::facade::ShareState;
    let who = user(7);
    let mut d = detail(&who);
    d.contact.as_mut().expect("contact").profile_name = Some("Ada".into());
    let v = view(&d);
    assert!(!v.share_shown && v.share_note.is_empty(), "hidden");
    d.share = ShareState::Ready;
    let v = view(&d);
    assert!(v.share_shown && v.share_note.is_empty());
    d.share = ShareState::NotShared;
    assert_eq!(view(&d).share_note, "Ada's phone number is not shown.");
    d.share = ShareState::NoNumber;
    assert_eq!(
        view(&d).share_note,
        "Kourier does not know Ada's phone number."
    );
    d.share = ShareState::NoName;
    let v = view(&d);
    assert!(v.share_shown);
    assert_eq!(v.share_note, "Ada has no name to share.");
}

/// P5c (SD9): the picker's line holds the name and the number of the card the CORE builds
/// (`PersonDetail::contact_card`), whichever name the page itself shows; with no card (no
/// profile name, a number not shown, not E.164, the own page, a network without cards) it is
/// empty. Synthetic name and number.
#[test]
fn the_card_line_holds_what_the_core_puts_into_the_card() {
    use kourier_core::facade::ShareState;
    let who = user(7);
    let mut d = detail(&who);
    d.share = ShareState::Ready;
    {
        let c = d.contact.as_mut().expect("contact");
        c.profile_name = Some("  Ada Profile ".into());
        c.nickname = Some("Ally".into());
        c.number_shown = Some(true);
        c.identifiers = vec![phone("+15550100")];
    }
    let card = d.contact_card().expect("a card");
    let line = view(&d).share_card;
    assert_eq!(line, "The card holds Ada Profile and +15550100.");
    // The same two values as the card itself: not the page's name (the nickname).
    let name = card.name.as_deref().expect("name");
    assert!(line.contains(name) && line.contains(&card.identifiers[0].value));
    assert_eq!(view(&d).name, "Ally");
    assert!(!line.contains("Ally"));

    // No card, no line: number not shown, never said, no profile name, a number that is no
    // E.164 one, a network without cards (Hidden), and the own page.
    let none = |d: &PersonDetail| {
        assert!(d.contact_card().is_none() || d.share == ShareState::Hidden);
        view(d).share_card.is_empty()
    };
    let mut x = d.clone();
    x.contact.as_mut().expect("contact").number_shown = Some(false);
    x.share = ShareState::NotShared;
    assert!(none(&x), "number not shown");
    x.contact.as_mut().expect("contact").number_shown = None;
    x.share = ShareState::NoNumber;
    assert!(none(&x), "never said");
    let mut x = d.clone();
    x.contact.as_mut().expect("contact").profile_name = None;
    x.share = ShareState::NoName;
    assert!(none(&x), "no profile name");
    let mut x = d.clone();
    x.contact.as_mut().expect("contact").identifiers = vec![phone("+1")];
    x.share = ShareState::NoNumber;
    assert!(none(&x), "no E.164 number");
    let mut x = d.clone();
    x.share = ShareState::Hidden;
    assert!(none(&x), "a network without cards");
    let mut x = d.clone();
    x.is_self = true;
    x.share = ShareState::Hidden;
    assert!(none(&x), "the own page");
    // The line is the card's alone: the page's other fields hold no copy of the picker's text.
    assert!(!format!("{:?}", view(&d)).contains("+15550100"));
}

/// P5c: the picker's chats come from the list the source holds, each with whether the user can
/// send there; "Send" is ONE `SendContact` naming the chat, the account and the person (the
/// card is the core's), and the core's refusal of it is a notice.
#[test]
fn share_targets_come_from_the_list_and_send_contact_names_chat_account_and_person() {
    let store = Store::new(&[(1, 1), (2, 1)]);
    let (mut src, sent) = source();
    src.reload();
    settle(&mut src, &sent, &store);
    for c in &mut src.list {
        c.can_send = c.id != ConversationId(2);
    }
    let targets = src.share_targets();
    assert_eq!(targets.len(), 2);
    let sendable: Vec<_> = targets
        .iter()
        .map(|t| (t.id.0, t.can_send, t.account))
        .collect();
    assert!(sendable.contains(&(1, true, AccountId(1))));
    assert!(sendable.contains(&(2, false, AccountId(1))));
    let listed = crate::share::listed_targets(&targets, AccountId(1), "");
    assert_eq!(
        listed.iter().map(|t| t.id.0).collect::<Vec<_>>(),
        vec![1],
        "a chat that cannot send is not listed"
    );

    sent.borrow_mut().clear();
    let who = user(7);
    assert!(src.send_contact(ConversationId(1), AccountId(1), who.clone()));
    let commands = sent.borrow();
    assert_eq!(commands.len(), 1);
    assert!(
        matches!(
            &commands[0],
            UiCommand::SendContact { conv, account, user }
                if *conv == ConversationId(1) && *account == AccountId(1) && *user == who
        ),
        "{:?}",
        commands[0]
    );
    drop(commands);

    let ch = src.on_event(&UiEvent::ActionFailed {
        account: Some(AccountId(1)),
        action: "SendContact",
        error: std::sync::Arc::new(CoreError::InvalidInput("no card")),
        subject: ActionSubject::Conversation(ConversationId(1)),
    });
    assert!(ch.notice);
    assert_eq!(src.notice(), "The contact card could not be sent.");
}

/// A copy action is a question of its own: it never waits for the page's, the answer is handed
/// over once, and when the user clicks twice the last click is what lands on the clipboard.
#[test]
fn a_copy_action_is_answered_once_and_only_the_newest_counts() {
    use crate::person::{CopyPart, CopyTarget};
    use kourier_core::facade::{LinkState, PersonRef};
    use kourier_model::Redacted;

    const NUMBER: &str = "+15550001111";
    const LINK: &str = "https://signal.me/#p/+15550001111";
    let who = user(7);
    let (mut src, sent) = opened_person(&Store::new(&[(1, 3)]));

    assert!(src.request_copy(CopyPart::Link, CopyTarget::Chat(ConversationId(5))));
    let older = take_queries(&sent).pop().expect("a query");
    assert_eq!(older.1, Query::PersonOf(PersonRef::Chat(ConversationId(5))));
    assert!(src.request_copy(CopyPart::Address, CopyTarget::Account(AccountId(1))));
    let newest = take_queries(&sent).pop().expect("a query");
    assert_eq!(newest.1, Query::PersonOf(PersonRef::Own(AccountId(1))));

    let mut d = detail(&who);
    d.is_self = true;
    d.own_number = Some(NUMBER.to_owned());
    d.link = LinkState::Link(Redacted::new(LINK.to_owned()));
    let ch = src.on_event(&result(
        older.0,
        QueryData::Person(Some(Box::new(d.clone()))),
    ));
    assert!(!ch.copy, "an older click's answer is dropped");
    assert!(src.take_copy().is_none());

    let ch = src.on_event(&result(
        newest.0,
        QueryData::Person(Some(Box::new(d.clone()))),
    ));
    assert!(ch.copy);
    let got = src.take_copy().expect("the answer");
    assert_eq!(got.text.as_ref().map(|t| t.expose().as_str()), Some(NUMBER));
    assert!(!got.told.contains(NUMBER), "the toast holds no number");
    assert!(src.take_copy().is_none(), "taken once");

    assert!(src.request_copy(
        CopyPart::Link,
        CopyTarget::Person {
            account: AccountId(1),
            user: who.clone()
        }
    ));
    let asked = take_queries(&sent).pop().expect("a query");
    assert!(matches!(asked.1, Query::Person { .. }));
    src.on_event(&result(asked.0, QueryData::Person(Some(Box::new(d)))));
    let got = src.take_copy().expect("the answer");
    assert_eq!(got.text.as_ref().map(|t| t.expose().as_str()), Some(LINK));
    assert_eq!(got.told, "Link copied");

    // A group's row, an account that is gone: nothing is copied and the toast says so.
    assert!(src.request_copy(CopyPart::Link, CopyTarget::Chat(ConversationId(6))));
    let asked = take_queries(&sent).pop().expect("a query");
    src.on_event(&result(asked.0, QueryData::Person(None)));
    let got = src.take_copy().expect("the answer");
    assert_eq!(got.text, None);
    assert_eq!(got.told, "No link to copy");
}

/// An action on a group's invite link: Copy and Reset put the link on the clipboard, turning it
/// on or off puts nothing there, the toast never holds the link and a failure says so.
#[test]
fn an_invite_action_copies_only_what_it_should() {
    use kourier_core::facade::CoreError;
    use kourier_core::facade::UiEvent;
    use kourier_model::{GroupInviteAction as Action, Redacted};

    const LINK: &str = "https://signal.group/#abc";
    const GROUP: ConversationId = ConversationId(9);
    let (mut src, sent) = opened_person(&Store::new(&[(1, 3)]));
    let link = || QueryData::GroupInvite(Some(Redacted::new(LINK.to_owned())));
    let asked = |src: &mut CoreSource, action| {
        assert!(src.request_group_invite(GROUP, action));
        let (token, query) = take_queries(&sent).pop().expect("a query");
        assert_eq!(
            query,
            Query::GroupInvite {
                conversation: GROUP,
                action
            }
        );
        token
    };

    for (action, told) in [
        (Action::Read, "Invite link copied"),
        (Action::Reset, "New invite link copied"),
    ] {
        let token = asked(&mut src, action);
        assert!(src.on_event(&result(token, link())).copy);
        let got = src.take_copy().expect("the answer");
        assert_eq!(got.text.as_ref().map(|t| t.expose().as_str()), Some(LINK));
        assert_eq!(got.told, told);
        assert!(
            !got.told.contains("signal.group"),
            "the toast holds no link"
        );
    }

    for (action, told) in [
        (Action::TurnOn, "The invite link is on"),
        (Action::TurnOff, "The invite link is off"),
    ] {
        let token = asked(&mut src, action);
        src.on_event(&result(token, link()));
        let got = src.take_copy().expect("the answer");
        assert_eq!(got.text, None, "nothing goes on the clipboard");
        assert_eq!(got.told, told);
    }

    // A link that is off: nothing to copy, and the toast says why.
    let token = asked(&mut src, Action::Read);
    src.on_event(&result(token, QueryData::GroupInvite(None)));
    let got = src.take_copy().expect("the answer");
    assert_eq!(got.text, None);
    assert_eq!(got.told, "The invite link is off");

    // The network could not be asked.
    let token = asked(&mut src, Action::Reset);
    src.on_event(&UiEvent::QueryResult {
        token,
        result: Err(std::sync::Arc::new(CoreError::Stopped)),
    });
    let got = src.take_copy().expect("the answer");
    assert_eq!(got.text, None);
    assert!(got.told.contains("could not be changed"), "{}", got.told);

    // An answer to an older click is dropped when a newer action is in flight.
    let older = asked(&mut src, Action::Read);
    let newest = asked(&mut src, Action::Read);
    assert!(!src.on_event(&result(older, link())).copy);
    assert!(src.take_copy().is_none());
    assert!(src.on_event(&result(newest, link())).copy);
}

/// What a copy action takes is what the page shows: a hidden number is not copied, a Threema ID
/// is the public address, and a link is taken only when the page has one.
#[test]
fn a_copy_action_takes_only_what_the_page_may_show() {
    use crate::person::{CopyPart, copy_update};
    use kourier_core::facade::LinkState;

    let who = user(7);
    let mut hidden = detail(&who);
    {
        let c = hidden.contact.as_mut().expect("contact");
        c.identifiers = vec![phone("+15550001111")];
        c.number_shown = Some(false);
    }
    hidden.link = LinkState::NotShared;
    let got = copy_update(Some(&hidden), CopyPart::Address, NetworkKind::Signal);
    assert_eq!(got.text, None, "a number that is not shared is not copied");
    assert_eq!(got.told, "No number or ID to copy");
    let got = copy_update(Some(&hidden), CopyPart::Link, NetworkKind::Signal);
    assert_eq!(got.text, None, "a page without a link copies none");

    let id = UserId::parse("threema:ABCD1234").expect("id");
    let got = copy_update(Some(&detail(&id)), CopyPart::Address, NetworkKind::Threema);
    assert_eq!(
        got.text.as_ref().map(|t| t.expose().as_str()),
        Some("ABCD1234"),
        "a Threema ID is the address people hand each other"
    );
    let got = copy_update(None, CopyPart::Address, NetworkKind::Threema);
    assert_eq!(got.text, None, "nobody there");
}
